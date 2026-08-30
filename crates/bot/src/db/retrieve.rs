//! [`PgRetriever`]: pipeline step 4 (ARCHITECTURE.md §3).
//!
//! `Context.rules` is the union of three legs, deduplicated by id in this
//! order: category map (curated subsections, always), full-text (BM25-like
//! `ts_rank_cd`), vector (pgvector cosine, only when an embedder is
//! configured and succeeds). Then Scryfall rulings and nightmare notes for
//! the cards, glossary entries whose term occurs in any face's Oracle text,
//! and up to five prior rated calls (same category, about one of the cards,
//! current CR version, not down-voted) as examples.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use judge_core::{
    CallId, Card, CardId, CardNote, Category, Context, CrVersion, Embedder, Extraction,
    GlossaryEntry, InputKind, JudgeError, PriorCall, Question, Retriever, RuleChunk, RuleId,
    Ruling,
};
use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

use super::{bad_row, rules, upstream};

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
    embedder: Option<Arc<dyn Embedder>>,
}

impl fmt::Debug for PgRetriever {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgRetriever")
            .field("embedder", &self.embedder.is_some())
            .finish_non_exhaustive()
    }
}

impl PgRetriever {
    /// A retriever without an embedder: the vector leg and similarity-ordered
    /// prior calls are skipped (with a warning) until [`Self::with_embedder`].
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            embedder: None,
        }
    }

    /// Enable the vector leg and similarity ordering of prior calls.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Embed the question, or `None` (logged) when no embedder is configured or it fails.
    async fn embed_query(&self, text: &str) -> Option<Vector> {
        let Some(embedder) = &self.embedder else {
            tracing::warn!(
                "no embedder configured; skipping the vector leg and prior-call similarity"
            );
            return None;
        };
        let first = match embedder.embed(&[text], InputKind::Query).await {
            Ok(vectors) => vectors.into_iter().next(),
            Err(e) => {
                tracing::warn!(error = %e, "embedding the question failed; skipping the vector leg");
                return None;
            }
        };
        if first.is_none() {
            tracing::warn!("embedder returned no vector; skipping the vector leg");
        }
        first.map(Vector::from)
    }

    /// Leg (a): every rule-level chunk of the subsections curated for `categories`.
    async fn category_map(&self, categories: &[Category]) -> Result<Vec<RuleChunk>, JudgeError> {
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
        let mut wanted: Vec<String> = Vec::new();
        for c in categories {
            if let Some(r) = rows.iter().find(|r| r.id == c.id()) {
                wanted.extend(r.subsections.iter().cloned());
            } else {
                tracing::warn!(category = %c, "no categories row; using the compiled subsection list");
                wanted.extend(c.subsections().iter().map(|s| (*s).to_owned()));
            }
        }
        let (subsections, exact) = rules::partition_ids(&wanted);
        rules::by_ids(&self.pool, &subsections, &exact).await
    }

    /// Leg (c): nearest rule-level chunks, when the question could be embedded.
    async fn nearest(&self, embedding: Option<&Vector>) -> Result<Vec<RuleChunk>, JudgeError> {
        match embedding {
            Some(v) => rules::nearest(&self.pool, v, VECTOR_LIMIT).await,
            None => Ok(Vec::new()),
        }
    }

    async fn rulings(&self, ids: &[Uuid]) -> Result<Vec<Ruling>, JudgeError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"
            SELECT oracle_id, idx, published_at::text AS "published_at!", text
            FROM rulings
            WHERE oracle_id = ANY($1)
            ORDER BY oracle_id, idx
            "#,
            ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("rulings"))?;
        rows.into_iter()
            .map(|r| {
                let idx = u32::try_from(r.idx).map_err(|_| {
                    bad_row(format!(
                        "negative ruling idx {} for card {}",
                        r.idx, r.oracle_id
                    ))
                })?;
                Ok(Ruling {
                    card: CardId::new(r.oracle_id),
                    idx,
                    published_at: r.published_at,
                    text: r.text,
                })
            })
            .collect()
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
    /// of `cards` (any call when no card resolved), under the current CR version
    /// (the newest `rules.cr_version`), excluding calls whose judge-aware
    /// `effective_score` is below 1.5 with five or more ratings; nearest-first
    /// when the question was embedded, else newest.
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
                      AND c.cr_version = (SELECT max(cr_version) FROM rules)
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
                      AND c.cr_version = (SELECT max(cr_version) FROM rules)
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
            self.category_map(&categories),
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
        let (n_map, n_bm25, n_vec) = (mapped.len(), matched.len(), nearest.len());
        ctx.extend_rules(mapped);
        ctx.extend_rules(matched);
        ctx.extend_rules(nearest);
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
