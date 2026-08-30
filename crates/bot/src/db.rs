//! sqlx adapters for the DB-backed ports: [`PgResolver`] (card resolution
//! ladder), [`PgRetriever`] (category map + BM25 + vector, rulings, glossary,
//! notes, prior calls) and [`PgCallStore`] (calls + ratings).
//!
//! Every query is a compile-time-checked `sqlx::query!` / `query_as!` against
//! `DATABASE_URL` (or the `.sqlx` offline cache); every sqlx error becomes
//! `JudgeError::Upstream` with a short context string.

mod calls;
mod cards;
mod resolve;
mod retrieve;
mod rules;
#[cfg(test)]
mod tests;

pub use calls::PgCallStore;
pub use resolve::PgResolver;
pub use retrieve::PgRetriever;

use judge_core::JudgeError;

/// Map a sqlx error to `JudgeError::Upstream`, naming the query that failed.
pub(crate) fn upstream(what: &'static str) -> impl FnOnce(sqlx::Error) -> JudgeError {
    move |e| JudgeError::Upstream(anyhow::Error::new(e).context(what))
}

/// A message-only data error (missing faces, negative idx) as `JudgeError::Upstream`.
pub(crate) fn bad_row(what: impl std::fmt::Display) -> JudgeError {
    JudgeError::Upstream(anyhow::anyhow!("{what}"))
}

/// Wrap a real error (serde, `RuleIdError`, …) as `JudgeError::Upstream` with
/// context, keeping the source chain for `{:#}` / `source()`.
pub(crate) fn bad_row_from(e: impl std::error::Error + Send + Sync + 'static, what: impl std::fmt::Display) -> JudgeError {
    JudgeError::Upstream(anyhow::Error::new(e).context(what.to_string()))
}
