//! [`PgLibrary`]: direct, read-only lookups over the same tables the
//! retriever draws on, for an agent that wants to consult the data rather
//! than run the pipeline: search the CR, read a card's rulings or notes, look
//! a glossary term up, fetch a card by id. Nothing here is a pipeline step
//! and nothing here writes.

use std::{fmt, sync::Arc};

use judge_core::{Card, CardId, CardNote, Embedder, GlossaryEntry, InputKind, JudgeError, RuleChunk, Ruling};
use pgvector::Vector;
use sqlx::PgPool;

use super::{cards, retrieve, rules, upstream};

/// Glossary entries returned for one term lookup at most.
pub const GLOSSARY_LIMIT: i64 = 10;
/// Most chunks one `search_rules` returns, whatever the caller asks.
pub const MAX_SEARCH: usize = 25;

/// Read-only lookups over cards, rules, rulings, glossary and notes.
#[derive(Clone)]
pub struct PgLibrary {
    pool: PgPool,
    embedder: Option<Arc<dyn Embedder>>,
}

impl fmt::Debug for PgLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLibrary").field("embedder", &self.embedder.is_some()).finish_non_exhaustive()
    }
}

impl PgLibrary {
    /// Without an embedder: `search_rules` is full-text only.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool, embedder: None }
    }

    /// Add the vector leg to `search_rules`.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Rule-level chunks matching `query`: the full-text leg, then (with an
    /// embedder) the nearest by cosine, unioned in that order and cut to
    /// `limit` (clamped to `1..=MAX_SEARCH`). The same two legs the retriever
    /// runs, minus the category map, which needs a classification. An empty
    /// query returns nothing and embeds nothing.
    ///
    /// # Errors
    /// `Upstream` from sqlx.
    pub async fn search_rules(&self, query: &str, limit: usize) -> Result<Vec<RuleChunk>, JudgeError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, MAX_SEARCH);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let matched = rules::bm25(&self.pool, query, query, limit_i64).await?;
        let mut out: Vec<RuleChunk> = Vec::with_capacity(limit);
        for c in matched {
            if !out.iter().any(|r| r.id == c.id) {
                out.push(c);
            }
        }
        if let Some(v) = self.embed(query).await {
            for c in rules::nearest(&self.pool, &v, limit_i64).await? {
                if !out.iter().any(|r| r.id == c.id) {
                    out.push(c);
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn embed(&self, text: &str) -> Option<Vector> {
        let embedder = self.embedder.as_ref()?;
        match embedder.embed(&[text], InputKind::Query).await {
            Ok(vectors) => vectors.into_iter().next().map(Vector::from),
            Err(e) => {
                tracing::warn!(error = %e, "embedding the search failed; full-text only");
                None
            }
        }
    }

    /// The card with this oracle id.
    ///
    /// # Errors
    /// `Upstream` from sqlx.
    pub async fn card(&self, id: CardId) -> Result<Option<Card>, JudgeError> {
        Ok(cards::load_cards(&self.pool, &[id.into_inner()]).await?.into_iter().next())
    }

    /// All Scryfall rulings of a card, newest first.
    ///
    /// # Errors
    /// `Upstream` from sqlx.
    pub async fn rulings(&self, card: CardId) -> Result<Vec<Ruling>, JudgeError> {
        retrieve::load_rulings(&self.pool, &[card.into_inner()]).await
    }

    /// The nightmare-card notes for a card.
    ///
    /// # Errors
    /// `Upstream` from sqlx.
    pub async fn notes(&self, card: CardId) -> Result<Vec<CardNote>, JudgeError> {
        let rows = sqlx::query!("SELECT note FROM card_notes WHERE oracle_id = $1", card.into_inner())
            .fetch_all(&self.pool)
            .await
            .map_err(upstream("card notes"))?;
        Ok(rows.into_iter().map(|r| CardNote { card, note: r.note }).collect())
    }

    /// Glossary entries whose term is `term` (case-insensitively), then those
    /// whose term contains it, at most [`GLOSSARY_LIMIT`].
    ///
    /// # Errors
    /// `Upstream` from sqlx.
    pub async fn glossary(&self, term: &str) -> Result<Vec<GlossaryEntry>, JudgeError> {
        let term = term.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        // Escape the LIKE metacharacters so a term is matched literally.
        let pattern = format!("%{}%", term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let rows = sqlx::query!(
            r#"
            SELECT term, text
            FROM glossary
            WHERE lower(term) = lower($1) OR term ILIKE $2
            ORDER BY (lower(term) = lower($1)) DESC, length(term), term
            LIMIT $3
            "#,
            term,
            pattern,
            GLOSSARY_LIMIT
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("glossary term"))?;
        Ok(rows.into_iter().map(|r| GlossaryEntry { term: r.term, text: r.text }).collect())
    }
}
