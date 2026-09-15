//! [`PgRetriever`]: pipeline step 4 (ARCHITECTURE.md §3).
//!
//! `Context.rules` is the union of three legs, deduplicated by id in priority
//! order ([`priority_order`]): the primary category's curated subsections
//! ranked by relevance to the question, full-text (BM25-like `ts_rank_cd`),
//! vector (pgvector cosine, only when a [`Vectors`] is configured, its space is
//! the stored one, and embedding succeeds), then the secondary categories,
//! also ranked. The synthesis prompt shows a prefix of it. Then Scryfall rulings and nightmare notes for
//! the cards, glossary entries whose term occurs in any face's Oracle text,
//! and up to five prior rated calls (same category, about one of the cards,
//! current CR version, not down-voted) as examples.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use judge_core::{
    CallId, Card, CardId, CardNote, Category, Context, CrVersion, Extraction, GlossaryEntry,
    InputKind, JudgeError, PriorCall, Question, Retriever, RuleChunk, RuleId, Ruling, RulingKey,
};
use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

use super::{Vectors, bad_row, rules, upstream};

/// Rule-level rows taken from the full-text leg.
pub const BM25_LIMIT: i64 = 12;
/// Rule-level rows taken from the vector leg.
pub const VECTOR_LIMIT: i64 = 12;
/// Prior calls shown as examples.
pub const PRIOR_LIMIT: i64 = 5;

/// Category map + tsvector BM25 + pgvector cosine, unioned and expanded to full chunks.
#[derive(Clone)]
pub struct PgRetriever {
    pool: PgPool,
    vectors: Option<Arc<Vectors>>,
}

impl fmt::Debug for PgRetriever {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgRetriever")
            .field("vectors", &self.vectors.as_ref().map(|v| v.space()))
            .finish_non_exhaustive()
    }
}

impl PgRetriever {
    /// A retriever without an embedder: the vector leg and similarity-ordered
    /// prior calls are skipped (with a warning) until [`Self::with_vectors`].
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            vectors: None,
        }
    }

    /// Enable the vector leg and similarity ordering of prior calls, subject
    /// to the space check in [`Vectors`].
    #[must_use]
    pub fn with_vectors(mut self, vectors: Arc<Vectors>) -> Self {
        self.vectors = Some(vectors);
        self
    }

    /// Embed the question, or `None` (logged) when no embedder is configured,
    /// its space is not the stored one, or it fails.
    async fn embed_query(&self, text: &str) -> Option<Vector> {
        let Some(vectors) = &self.vectors else {
            tracing::warn!(
                "no embedder configured; skipping the vector leg and prior-call similarity"
            );
            return None;
        };
        vectors.embed(text, InputKind::Query).await
    }

    /// Leg (a): the rule-level chunks of the subsections curated for each of
    /// `categories`, one list per category in the given (priority) order, each
    /// ranked by relevance to the question.
    async fn category_map(
        &self,
        categories: &[Category],
        concepts: &str,
        question: &str,
    ) -> Result<Vec<rules::CategoryRules>, JudgeError> {
        if categories.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = categories.iter().map(|c| c.id().to_owned()).collect();
        let rows = sqlx::query!(
            "SELECT id, subsections FROM categories WHERE id = ANY($1)",
            &ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("categories"))?;
        let mut legs = Vec::with_capacity(categories.len());
        for c in categories {
            let wanted: Vec<String> = if let Some(r) = rows.iter().find(|r| r.id == c.id()) {
                r.subsections.clone()
            } else {
                tracing::warn!(category = %c, "no categories row; using the compiled subsection list");
                c.subsections().iter().map(|s| (*s).to_owned()).collect()
            };
            let (subsections, exact) = rules::partition_ids(&wanted);
            legs.push(
                rules::in_subsections_ranked(&self.pool, &subsections, &exact, concepts, question)
                    .await?,
            );
        }
        Ok(legs)
    }

    /// Leg (c): nearest rule-level chunks, when the question could be embedded.
    async fn nearest(&self, embedding: Option<&Vector>) -> Result<Vec<RuleChunk>, JudgeError> {
        match embedding {
            Some(v) => rules::nearest(&self.pool, v, VECTOR_LIMIT).await,
            None => Ok(Vec::new()),
        }
    }

    async fn rulings(&self, ids: &[Uuid]) -> Result<Vec<Ruling>, JudgeError> {
        load_rulings(&self.pool, ids).await
    }

    /// Glossary entries whose term (or, for compound headwords such as
    /// "Control, Controller", any comma-separated part of it) occurs as whole
    /// words (case-insensitive, punctuation-insensitive) in any of `texts`.
    async fn glossary(&self, texts: &[String]) -> Result<Vec<GlossaryEntry>, JudgeError> {
        if texts.iter().all(|t| t.trim().is_empty()) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"
            WITH txt AS (
                SELECT ' ' || lower(regexp_replace(t, '[^[:alnum:]]+', ' ', 'g')) || ' ' AS norm
                FROM unnest($1::text[]) AS u(t)
            ), g AS (
                -- Compound headwords ("Control, Controller", "Base Power, Base Toughness")
                -- match on any of their comma-separated parts; "(Obsolete)" is dropped.
                SELECT term, text, trim(lower(regexp_replace(part, '[^[:alnum:]]+', ' ', 'g'))) AS norm_term
                FROM glossary,
                     regexp_split_to_table(regexp_replace(term, '\s*\(Obsolete\)$', ''), '\s*,\s*') AS part
            )
            SELECT DISTINCT g.term AS "term!", g.text AS "text!"
            FROM g
            WHERE g.norm_term <> ''
              AND EXISTS (SELECT 1 FROM txt WHERE position((' ' || g.norm_term || ' ') IN txt.norm) > 0)
            ORDER BY g.term
            "#,
            texts
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("glossary"))?;
        Ok(rows
            .into_iter()
            .map(|r| GlossaryEntry {
                term: r.term,
                text: r.text,
            })
            .collect())
    }

    async fn notes(&self, ids: &[Uuid]) -> Result<Vec<CardNote>, JudgeError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            "SELECT oracle_id, note FROM card_notes WHERE oracle_id = ANY($1)",
            ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("card notes"))?;
        Ok(rows
            .into_iter()
            .map(|r| CardNote {
                card: CardId::new(r.oracle_id),
                note: r.note,
            })
            .collect())
    }

    /// Prior calls in any of `categories` that were answered about at least one
    /// of `cards` (any call when no card resolved), not retired (every citation
    /// still supported by the current data — see `retire.rs`), excluding calls
    /// whose judge-aware `effective_score` is below 1.5 with five or more
    /// ratings, and excluding calls persisted from agent sessions
    /// (`session_id` set): their citations were validated but their answer
    /// text is an outside agent's, and nothing can rate them (no Discord
    /// message to vote on), so they serve as history for their own thread
    /// and never as examples for other askers. Nearest-first when the
    /// question was embedded, else newest.
    async fn prior_calls(
        &self,
        categories: &[Category],
        cards: &[Uuid],
        embedding: Option<&Vector>,
    ) -> Result<Vec<PriorCall>, JudgeError> {
        let cats: Vec<String> = if categories.is_empty() {
            vec![Category::Other.id().to_owned()]
        } else {
            categories.iter().map(|c| c.id().to_owned()).collect()
        };
        let rows = match embedding {
            Some(v) => {
                sqlx::query_as!(
                    PriorRow,
                    r#"
                    SELECT c.id, c.question, c.answer, c.category, c.citations, c.cr_version,
                           r.effective_score AS "rating!", r.n AS "n!"
                    FROM calls c
                    JOIN calls_rated r ON r.call_id = c.id
                    WHERE c.category = ANY($1)
                      AND c.retired_at IS NULL
                      AND c.session_id IS NULL
                      AND NOT (r.effective_score < 1.5 AND r.n >= 5)
                      AND (cardinality($4::uuid[]) = 0 OR EXISTS (
                            SELECT 1 FROM jsonb_array_elements_text(c.context_ids->'cards') AS x(v)
                            WHERE x.v::uuid = ANY($4)))
                    ORDER BY c.embedding <=> $2, c.created_at DESC
                    LIMIT $3
                    "#,
                    &cats,
                    v as _,
                    PRIOR_LIMIT,
                    cards
                )
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query_as!(
                    PriorRow,
                    r#"
                    SELECT c.id, c.question, c.answer, c.category, c.citations, c.cr_version,
                           r.effective_score AS "rating!", r.n AS "n!"
                    FROM calls c
                    JOIN calls_rated r ON r.call_id = c.id
                    WHERE c.category = ANY($1)
                      AND c.retired_at IS NULL
                      AND c.session_id IS NULL
                      AND NOT (r.effective_score < 1.5 AND r.n >= 5)
                      AND (cardinality($3::uuid[]) = 0 OR EXISTS (
                            SELECT 1 FROM jsonb_array_elements_text(c.context_ids->'cards') AS x(v)
                            WHERE x.v::uuid = ANY($3)))
                    ORDER BY c.created_at DESC
                    LIMIT $2
                    "#,
                    &cats,
                    PRIOR_LIMIT,
                    cards
                )
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(upstream("prior calls"))?;
        Ok(rows
            .into_iter()
            .filter_map(PriorRow::into_prior_call)
            .collect())
    }
}

struct PriorRow {
    id: Uuid,
    question: String,
    answer: String,
    category: String,
    citations: serde_json::Value,
    cr_version: String,
    rating: f32,
    n: i32,
}

impl PriorRow {
    /// A corrupt row is logged and skipped rather than failing retrieval.
    fn into_prior_call(self) -> Option<PriorCall> {
        let id = CallId::new(self.id);
        let category = match self.category.parse::<Category>() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(call = %id, error = %e, "skipping prior call with unknown category");
                return None;
            }
        };
        let cr_version = match CrVersion::try_new(self.cr_version) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(call = %id, error = %e, "skipping prior call with bad cr_version");
                return None;
            }
        };
        let citations = serde_json::from_value(self.citations).unwrap_or_else(|e| {
            tracing::warn!(call = %id, error = %e, "prior call has undecodable citations; using none");
            Vec::new()
        });
        Some(PriorCall {
            id,
            question: self.question,
            answer: self.answer,
            category,
            citations,
            cr_version,
            rating: self.rating,
            rating_count: u32::try_from(self.n).unwrap_or(0),
        })
    }
}

/// All rulings of these cards, newest first within a card.
pub(super) async fn load_rulings(
    pool: impl sqlx::PgExecutor<'_>,
    ids: &[Uuid],
) -> Result<Vec<Ruling>, JudgeError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT oracle_id, key, to_char(published_at, 'YYYY-MM-DD') AS "published_at!", text
        FROM rulings
        WHERE oracle_id = ANY($1)
        ORDER BY oracle_id, published_at DESC, key
        "#,
        ids
    )
    .fetch_all(pool)
    .await
    .map_err(upstream("rulings"))?;
    rows.into_iter()
        .map(|r| {
            let key: RulingKey = r
                .key
                .parse()
                .map_err(|e| bad_row(format!("card {}: {e}", r.oracle_id)))?;
            Ok(Ruling {
                card: CardId::new(r.oracle_id),
                key,
                published_at: r.published_at,
                text: r.text,
            })
        })
        .collect()
}

#[async_trait]
impl Retriever for PgRetriever {
    async fn retrieve(
        &self,
        q: &Question,
        cards: &[Card],
        e: &Extraction,
    ) -> Result<Context, JudgeError> {
        let categories: Vec<Category> = e.categories().map(|g| g.category).collect();
        let concepts = e.concepts.join(" ");
        let embedding = self.embed_query(&q.text).await;
        let card_ids: Vec<Uuid> = cards.iter().map(|c| c.id.into_inner()).collect();
        let texts: Vec<String> = cards
            .iter()
            .flat_map(|c| c.faces.iter().map(|f| f.oracle_text.clone()))
            .collect();

        let (mapped, matched, nearest, rulings, glossary, notes, prior) = tokio::try_join!(
            self.category_map(&categories, &concepts, &q.text),
            rules::bm25(&self.pool, &concepts, &q.text, BM25_LIMIT),
            self.nearest(embedding.as_ref()),
            self.rulings(&card_ids),
            self.glossary(&texts),
            self.notes(&card_ids),
            self.prior_calls(&categories, &card_ids, embedding.as_ref()),
        )?;

        let mut ctx = Context {
            cards: cards.to_vec(),
            rulings,
            glossary,
            notes,
            prior,
            ..Context::default()
        };
        let n_map = mapped
            .iter()
            .map(|c| c.matching.len() + c.rest.len())
            .sum::<usize>();
        let (n_bm25, n_vec) = (matched.len(), nearest.len());
        ctx.extend_rules(priority_order(mapped, matched, nearest));
        tracing::info!(
            category_map = n_map,
            full_text = n_bm25,
            vector = n_vec,
            rules = ctx.rules.len(),
            rulings = ctx.rulings.len(),
            glossary = ctx.glossary.len(),
            notes = ctx.notes.len(),
            prior = ctx.prior.len(),
            "context built"
        );
        Ok(ctx)
    }

    async fn lookup_rules(&self, ids: &[RuleId]) -> Result<Vec<RuleChunk>, JudgeError> {
        let (subsections, exact) = rules::partition_ids(ids.iter().map(ToString::to_string));
        rules::by_ids(&self.pool, &subsections, &exact).await
    }
}

/// `Context.rules` in the order the synthesis prompt fills its budget from:
///
/// 1. the primary category's rules that share a word with the question, most
///    relevant first;
/// 2. the full-text hits, then the vector hits;
/// 3. the primary category's remaining rules;
/// 4. each secondary category (matching rules, then the rest), in the
///    classifier's order;
///
/// deduplicated by id, first occurrence winning.
///
/// The budget renders a *prefix* of this list (`synth::shown_rules`) and the
/// legs together return several times what it shows, so this order decides
/// what the model reads; `eval recall` scores exactly that. It is a priority
/// order, not a relevance merge: a large primary category whose rules all
/// share some word with the question still fills the budget before a better
/// full-text hit (the gold trample + deathtouch question under `combat`). That
/// trade was measured. On the gold set, with and without the vector leg, this
/// order shows 81% of the expected rules; merging the primary category with
/// the full-text hits by score 55-76% (the top full-text hits are long general
/// rules such as 608.2), round-robin across legs 67-70%, capping the primary
/// category at 10-15 chunks 75-78%, and the old id-sorted union of all
/// categories 43%. 97% were retrieved in every case.
fn priority_order(
    categories: Vec<rules::CategoryRules>,
    matched: Vec<RuleChunk>,
    nearest: Vec<RuleChunk>,
) -> Vec<RuleChunk> {
    let mut categories = categories.into_iter();
    let primary = categories.next().unwrap_or_default();
    let legs = [primary.matching, matched, nearest, primary.rest]
        .into_iter()
        .chain(categories.flat_map(|c| [c.matching, c.rest]));
    let mut out: Vec<RuleChunk> = Vec::new();
    for c in legs.flatten() {
        if !out.iter().any(|r| r.id == c.id) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rules::CategoryRules;

    fn chunks(ids: &[&str]) -> Result<Vec<RuleChunk>, Box<dyn std::error::Error>> {
        ids.iter()
            .map(|id| {
                Ok(RuleChunk {
                    id: RuleId::try_new((*id).to_owned())?,
                    parent_id: None,
                    subsection: RuleId::try_new(id.get(..3).unwrap_or("100").to_owned())?,
                    heading: String::new(),
                    body: String::new(),
                    examples: vec![],
                    cr_version: CrVersion::try_new("20260819".to_owned())?,
                })
            })
            .collect()
    }

    fn ids(rules: &[RuleChunk]) -> Vec<&str> {
        rules.iter().map(|r| r.id.as_ref()).collect()
    }

    #[test]
    fn primary_then_full_text_then_vector_then_secondaries_first_occurrence_wins()
    -> Result<(), Box<dyn std::error::Error>> {
        let category = |matching: &[&str],
                        rest: &[&str]|
         -> Result<CategoryRules, Box<dyn std::error::Error>> {
            Ok(CategoryRules {
                matching: chunks(matching)?,
                rest: chunks(rest)?,
            })
        };
        let ordered = priority_order(
            vec![
                category(&["709.5", "709.1"], &["710.1"])?,
                category(&["100.1", "702.19"], &["101.1"])?,
                category(&[], &["205.3"])?,
            ],
            chunks(&["702.19", "709.5"])?,
            chunks(&["708.4", "100.1"])?,
        );
        assert_eq!(
            ids(&ordered),
            [
                "709.5", "709.1", "702.19", "708.4", "100.1", "710.1", "101.1", "205.3"
            ]
        );
        assert!(priority_order(vec![], vec![], vec![]).is_empty());
        assert_eq!(
            ids(&priority_order(vec![], chunks(&["702.19"])?, vec![])),
            ["702.19"]
        );
        Ok(())
    }
}
