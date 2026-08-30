//! The `recall` evaluator: is every expected rule id in the retrieved Context?

use std::fmt;

use judge_core::{Context, Resolution, Resolver, Retriever};
use judge_bot::db::{PgResolver, PgRetriever};
use sqlx::PgPool;

use crate::gold::Gold;

/// Per-question outcome.
pub struct Row {
    /// Gold question id.
    pub id: String,
    /// Resolution outcome per card name / nickname, e.g. `Dark Confidant=alias`.
    pub resolutions: Vec<String>,
    /// Expected ids found in Context (leaf ids count if their rule row is present).
    pub hit: Vec<String>,
    /// Expected ids not in Context.
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
    /// Fraction of expected rule ids present, over all scored questions (0 when nothing was expected).
    #[must_use]
    pub fn recall(&self) -> f64 {
        let hit: usize = self.rows.iter().map(|r| r.hit.len()).sum();
        let total: usize = self.rows.iter().map(|r| r.hit.len() + r.missed.len()).sum();
        if total == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let recall = hit as f64 / total as f64;
        recall
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self.rows.iter().map(|r| r.id.len()).max().unwrap_or(8).max(8);
        writeln!(f, "{:<width$}  {:>5}  {:<40}  cards", "question", "hit", "missed")?;
        for r in &self.rows {
            let total = r.hit.len() + r.missed.len();
            writeln!(
                f,
                "{:<width$}  {:>2}/{:<2}  {:<40}  {}",
                r.id,
                r.hit.len(),
                total,
                r.missed.join(" "),
                r.resolutions.join(", ")
            )?;
        }
        let hit: usize = self.rows.iter().map(|r| r.hit.len()).sum();
        let total: usize = self.rows.iter().map(|r| r.hit.len() + r.missed.len()).sum();
        writeln!(
            f,
            "\nrecall: {hit}/{total} = {:.1}% over {} questions ({} skipped as out of scope)",
            self.recall() * 100.0,
            self.rows.len(),
            self.skipped
        )
    }
}

/// Run the evaluator over `gold`.
///
/// # Errors
/// On database failure.
pub async fn run(pool: &PgPool, gold: &Gold) -> anyhow::Result<Report> {
    let resolver = PgResolver::new(pool.clone());
    let retriever = PgRetriever::new(pool.clone());
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
        let question = judge_core::Question { thread_id: q.id.clone(), text: q.question.clone() };
        let ctx = retriever.retrieve(&question, &cards, &extraction).await?;
        let (mut hit, mut missed) = (Vec::new(), Vec::new());
        for expected in q.expected_rule_ids.iter().map(super::gold::YamlScalar::as_text) {
            if present(&ctx, &expected) { hit.push(expected) } else { missed.push(expected) }
        }
        rows.push(Row { id: q.id.clone(), resolutions, hit, missed });
    }
    Ok(Report { rows, skipped })
}

/// A leaf id (`613.1d`) counts if the leaf row or its rule row (`613.1`) is in Context.
fn present(ctx: &Context, id: &str) -> bool {
    let rule = id.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    ctx.rules.iter().any(|r| {
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
        Resolution::Ambiguous { candidates, .. } => format!("{name}=AMBIGUOUS({})", candidates.len()),
        Resolution::NotFound { .. } => format!("{name}=NOT_FOUND"),
    }
}
