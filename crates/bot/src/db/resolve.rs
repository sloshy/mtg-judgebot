//! [`PgResolver`]: the card resolution ladder (ARCHITECTURE.md §3 step 2).
//!
//! `card_aliases` → `[[bracket]]` exact name → exact current name →
//! `printed_names` → `pg_trgm` fuzzy (the `judge_core::MatchedVia` order).
//! Exact rungs match case-insensitively on both `cards.name` and
//! `card_faces.name` (so "Stomp" finds "Bonecrusher Giant // Stomp"); every
//! current name is also a printed name, so `Exact` runs first to keep `via`
//! informative (an errata'd old name reports `PrintedName`).
//! The fuzzy rung scores with `strict_word_similarity`, so a partial name such
//! as "urza" scores 1.0 against every "Urza's …" card and comes back
//! `Ambiguous`, while a short span buried inside a word ("led" in "Grizzled")
//! does not match; the ladder never guesses. When an exact rung matches several
//! cards, one whose *full* name is the span wins over face-name matches
//! ("Lightning Bolt" beats "Emeritus of Conflict // Lightning Bolt").

use async_trait::async_trait;
use judge_core::{JudgeError, MatchedVia, Resolution, Resolver};
use nonempty::NonEmpty;
use sqlx::PgPool;
use uuid::Uuid;

use super::{bad_row, cards::load_cards, upstream};

/// Lowest `strict_word_similarity` a fuzzy candidate must reach to be considered.
pub const FUZZY_LOW: f32 = 0.45;
/// A single candidate at or above this similarity is accepted outright.
pub const FUZZY_STRONG: f32 = 0.7;
/// The top candidate is accepted if it leads the runner-up by at least this much.
pub const FUZZY_MARGIN: f32 = 0.15;
/// Most candidates offered in a "did you mean…?".
pub const MAX_CANDIDATES: usize = 5;
/// Fetch one more than offered so the margin rule can see the runner-up.
const FUZZY_FETCH: i64 = 6;

/// alias table → `[[bracket]]` syntax → printed-name table → `pg_trgm` fuzzy.
#[derive(Clone, Debug)]
pub struct PgResolver {
    pool: PgPool,
}

impl PgResolver {
    /// A resolver over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn alias(&self, lowered: &str) -> Result<Option<Uuid>, JudgeError> {
        sqlx::query_scalar!("SELECT oracle_id FROM card_aliases WHERE alias = $1", lowered)
            .fetch_optional(&self.pool)
            .await
            .map_err(upstream("alias lookup"))
    }

    /// Case-insensitive exact match on current card names and face names.
    async fn exact_name(&self, lowered: &str) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"
            SELECT oracle_id AS "oracle_id!" FROM cards WHERE lower(name) = $1
            UNION
            SELECT oracle_id FROM card_faces WHERE lower(name) = $1
            "#,
            lowered
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("exact name lookup"))
    }

    async fn printed_name(&self, lowered: &str) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT oracle_id AS "oracle_id!" FROM printed_names WHERE lower(printed_name) = $1"#,
            lowered
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("printed name lookup"))
    }

    /// Among `ids`, those whose full card name is exactly `lowered` (case-insensitive).
    async fn full_name_matches(&self, lowered: &str, ids: &[Uuid]) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"SELECT oracle_id AS "oracle_id!" FROM cards WHERE lower(name) = $1 AND oracle_id = ANY($2)"#,
            lowered,
            ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("full name lookup"))
    }

    /// Candidates by `strict_word_similarity`, best first, deduplicated per card.
    async fn fuzzy_candidates(&self, text: &str) -> Result<Vec<(Uuid, f32)>, JudgeError> {
        let mut tx = self.pool.begin().await.map_err(upstream("begin fuzzy lookup"))?;
        // `<%` filters through the trigram GIN indexes at this (transaction-local) threshold.
        sqlx::query_scalar!("SELECT set_config('pg_trgm.strict_word_similarity_threshold', $1, true)", FUZZY_LOW.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(upstream("set strict_word_similarity_threshold"))?;
        let rows = sqlx::query!(
            r#"
            WITH cand AS (
                SELECT oracle_id, strict_word_similarity($1, name) AS sim FROM cards WHERE $1 <<% name
                UNION ALL
                SELECT oracle_id, strict_word_similarity($1, name) FROM card_faces WHERE $1 <<% name
            )
            SELECT cand.oracle_id AS "oracle_id!", max(cand.sim)::real AS "sim!"
            FROM cand
            JOIN cards c ON c.oracle_id = cand.oracle_id
            GROUP BY cand.oracle_id, c.name
            HAVING max(cand.sim) >= $2
            ORDER BY max(cand.sim) DESC, length(c.name), c.name
            LIMIT $3
            "#,
            text,
            FUZZY_LOW,
            FUZZY_FETCH
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(upstream("fuzzy lookup"))?;
        tx.commit().await.map_err(upstream("end fuzzy lookup"))?;
        Ok(rows.into_iter().map(|r| (r.oracle_id, r.sim)).collect())
    }

    async fn resolved(&self, query: &str, id: Uuid, via: MatchedVia) -> Result<Resolution, JudgeError> {
        let card = load_cards(&self.pool, &[id])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| bad_row(format!("card {id} matched {query:?} but has no cards/card_faces row")))?;
        tracing::info!(span = query, card = %card.name, rung = ?via, "card resolved");
        Ok(Resolution::Resolved { card, via })
    }

    /// `None` when `ids` is empty (fall through to the next rung). Several
    /// matches are still unambiguous when exactly one of them is a card whose
    /// full name is the span (the others matched on a face or printed name).
    async fn decide(
        &self,
        query: &str,
        lowered: &str,
        ids: Vec<Uuid>,
        via: MatchedVia,
    ) -> Result<Option<Resolution>, JudgeError> {
        match ids.as_slice() {
            [] => Ok(None),
            [id] => self.resolved(query, *id, via).await.map(Some),
            _ => {
                if let [id] = self.full_name_matches(lowered, &ids).await?.as_slice() {
                    return self.resolved(query, *id, via).await.map(Some);
                }
                self.ambiguous(query, &ids, via).await.map(Some)
            }
        }
    }

    async fn ambiguous(&self, query: &str, ids: &[Uuid], rung: MatchedVia) -> Result<Resolution, JudgeError> {
        let ids: Vec<Uuid> = ids.iter().copied().take(MAX_CANDIDATES).collect();
        let cards = load_cards(&self.pool, &ids).await?;
        tracing::info!(span = query, candidates = cards.len(), rung = ?rung, "card ambiguous");
        Ok(match NonEmpty::from_vec(cards) {
            Some(candidates) => Resolution::Ambiguous { query: query.to_owned(), candidates },
            None => Resolution::NotFound { query: query.to_owned() },
        })
    }

    async fn fuzzy(&self, query: &str, text: &str) -> Result<Resolution, JudgeError> {
        let cands = self.fuzzy_candidates(text).await?;
        let winner = match cands.as_slice() {
            [] => None,
            [(id, _)] => Some(*id),
            [(id, top), (_, second), ..] => {
                let strong = cands.iter().filter(|(_, s)| *s >= FUZZY_STRONG).count();
                ((strong == 1 && *top >= FUZZY_STRONG) || top - second >= FUZZY_MARGIN).then_some(*id)
            }
        };
        if let Some(id) = winner {
            return self.resolved(query, id, MatchedVia::Fuzzy).await;
        }
        if cands.is_empty() {
            tracing::info!(span = query, "card not found");
            return Ok(Resolution::NotFound { query: query.to_owned() });
        }
        let ids: Vec<Uuid> = cands.iter().map(|(id, _)| *id).collect();
        self.ambiguous(query, &ids, MatchedVia::Fuzzy).await
    }
}

/// `[[Card Name]]` → (`Card Name`, true); anything else → (as is, false).
fn strip_brackets(s: &str) -> (&str, bool) {
    s.strip_prefix("[[")
        .and_then(|inner| inner.strip_suffix("]]"))
        .map_or((s, false), |inner| (inner.trim(), true))
}

#[async_trait]
impl Resolver for PgResolver {
    async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError> {
        let query = span.trim().to_owned();
        let (text, bracketed) = strip_brackets(&query);
        let lowered = text.to_lowercase();
        if lowered.is_empty() {
            return Ok(Resolution::NotFound { query });
        }
        if let Some(id) = self.alias(&lowered).await? {
            return self.resolved(&query, id, MatchedVia::Alias).await;
        }
        if bracketed {
            let ids = self.exact_name(&lowered).await?;
            if let Some(r) = self.decide(&query, &lowered, ids, MatchedVia::Bracket).await? {
                return Ok(r);
            }
        }
        if !bracketed {
            let ids = self.exact_name(&lowered).await?;
            if let Some(r) = self.decide(&query, &lowered, ids, MatchedVia::Exact).await? {
                return Ok(r);
            }
        }
        let ids = self.printed_name(&lowered).await?;
        if let Some(r) = self.decide(&query, &lowered, ids, MatchedVia::PrintedName).await? {
            return Ok(r);
        }
        self.fuzzy(&query, text).await
    }
}

#[cfg(test)]
mod unit {
    use super::strip_brackets;

    #[test]
    fn brackets() {
        assert_eq!(strip_brackets("[[ Dark Confidant ]]"), ("Dark Confidant", true));
        assert_eq!(strip_brackets("Dark Confidant"), ("Dark Confidant", false));
        assert_eq!(strip_brackets("[[oops"), ("[[oops", false));
    }
}
