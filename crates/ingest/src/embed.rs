//! Fill NULL `embedding` columns on `rules`, `glossary` and `calls` via the `Embedder`.
//!
//! Only rule-level `rules` rows (`parent_id IS NULL`) are embedded: the retriever's
//! vector leg returns rule chunks only (leaves are folded into their rule's body), and
//! the HNSW index is partial over exactly those rows.
//!
//! Each table is walked in batches of at most [`MAX_BATCH`] rows (Voyage's per-request
//! limit) whose `embedding IS NULL`; texts are embedded with `InputKind::Document` and
//! written back as `pgvector` vectors. Without an embedder nothing is touched.

use anyhow::Context as _;
use judge_core::{Embedder, InputKind};
use pgvector::Vector;
use sqlx::{PgPool, Row as _};

/// Voyage accepts at most 128 texts per request.
const MAX_BATCH: usize = 128;
/// Free-tier Voyage allows only 10K tokens/request-minute; `VOYAGE_MAX_BATCH`
/// lets an ingest run shrink batches to fit (default: [`MAX_BATCH`]).
fn max_batch() -> usize {
    std::env::var("VOYAGE_MAX_BATCH").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(MAX_BATCH)
}
/// Soft cap on characters per request so a batch of long rule chunks stays under the
/// provider's token budget (voyage-3.5: 320k tokens; ~4 chars/token, with margin).
const MAX_BATCH_CHARS: usize = 400_000;
/// `vector(1024)` columns in the schema.
const EXPECTED_DIMENSIONS: usize = 1024;

/// One embeddable table: how to select unembedded rows and write vectors back.
/// Keys are selected as `text` so one loop serves `text` and `uuid` primary keys.
struct Target {
    name: &'static str,
    select: &'static str,
    update: &'static str,
}

const TARGETS: [Target; 3] = [
    Target {
        name: "rules",
        select: "SELECT id::text AS key, heading || E'\\n' || body || \
                 CASE WHEN cardinality(examples) > 0 THEN E'\\nExample: ' || array_to_string(examples, E'\\nExample: ') ELSE '' END AS txt \
                 FROM rules WHERE embedding IS NULL AND parent_id IS NULL ORDER BY id LIMIT $1",
        update: "UPDATE rules SET embedding = $1 WHERE id = $2 AND embedding IS NULL",
    },
    Target {
        name: "glossary",
        select: "SELECT term AS key, term || E'\\n' || text AS txt FROM glossary WHERE embedding IS NULL ORDER BY term LIMIT $1",
        update: "UPDATE glossary SET embedding = $1 WHERE term = $2 AND embedding IS NULL",
    },
    Target {
        name: "calls",
        select: "SELECT id::text AS key, question || E'\\n' || answer AS txt FROM calls WHERE embedding IS NULL ORDER BY id LIMIT $1",
        update: "UPDATE calls SET embedding = $1 WHERE id = $2::uuid AND embedding IS NULL",
    },
];

/// Embed every row whose `embedding IS NULL`. With no embedder configured this
/// logs a warning and does nothing.
///
/// # Errors
/// On embedding or database failure.
pub async fn run(pool: &PgPool, embedder: Option<&dyn Embedder>) -> anyhow::Result<()> {
    let Some(embedder) = embedder else {
        tracing::warn!("ingest embed: skipped, no embedder configured (set VOYAGE_API_KEY)");
        return Ok(());
    };
    if embedder.dimensions() != EXPECTED_DIMENSIONS {
        anyhow::bail!(
            "embedder produces {} dimensions but the schema's vector columns are vector({EXPECTED_DIMENSIONS}); set VOYAGE_DIMENSIONS={EXPECTED_DIMENSIONS}",
            embedder.dimensions()
        );
    }
    for t in &TARGETS {
        let n = embed_table(pool, embedder, t).await.with_context(|| format!("embedding {}", t.name))?;
        tracing::info!(table = t.name, embedded = n, "ingest embed");
    }
    Ok(())
}

/// Embed `t` until no NULL rows remain; returns the number of rows embedded.
async fn embed_table(pool: &PgPool, embedder: &dyn Embedder, t: &Target) -> anyhow::Result<usize> {
    let mut total = 0usize;
    loop {
        let rows = sqlx::query(t.select)
            .bind(i64::try_from(max_batch())?)
            .fetch_all(pool)
            .await
            .context("selecting unembedded rows")?;
        if rows.is_empty() {
            return Ok(total);
        }
        let mut pairs: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for r in &rows {
            let key: String = r.try_get("key")?;
            let txt: String = r.try_get("txt")?;
            // An empty text would be rejected by the provider; embed a placeholder instead.
            let txt = if txt.trim().is_empty() { key.clone() } else { txt };
            pairs.push((key, txt));
        }
        let batch = fit_chars(&pairs);
        let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = embedder.embed(&texts, InputKind::Document).await.map_err(|e| anyhow::anyhow!("embedder: {e}"))?;
        if vectors.len() != batch.len() {
            anyhow::bail!("embedder returned {} vectors for {} texts", vectors.len(), batch.len());
        }
        let mut written = 0u64;
        let mut tx = pool.begin().await?;
        for ((key, _), v) in batch.iter().zip(vectors) {
            if v.len() != EXPECTED_DIMENSIONS {
                anyhow::bail!("embedding for {} {key} has {} dimensions, expected {EXPECTED_DIMENSIONS}", t.name, v.len());
            }
            written += sqlx::query(t.update).bind(Vector::from(v)).bind(key).execute(&mut *tx).await?.rows_affected();
        }
        tx.commit().await?;
        if written == 0 {
            // Nothing was updated although rows were selected: bail rather than spin.
            anyhow::bail!("{}: selected {} unembedded rows but updated none", t.name, batch.len());
        }
        total += usize::try_from(written)?;
        tracing::debug!(table = t.name, batch = batch.len(), total, "embedded batch");
    }
}

/// The longest prefix of `pairs` (at least one) whose texts total ≤ `MAX_BATCH_CHARS`.
fn fit_chars(pairs: &[(String, String)]) -> &[(String, String)] {
    let mut chars = 0usize;
    let mut n = 0usize;
    for (_, t) in pairs {
        chars += t.len();
        if n > 0 && chars > MAX_BATCH_CHARS {
            break;
        }
        n += 1;
    }
    pairs.get(..n).unwrap_or(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_chars_keeps_at_least_one_and_respects_cap() {
        let big = "x".repeat(MAX_BATCH_CHARS);
        let pairs = vec![("a".to_owned(), big.clone()), ("b".to_owned(), big), ("c".to_owned(), "s".to_owned())];
        assert_eq!(fit_chars(&pairs).len(), 1);
        let small: Vec<(String, String)> = (0..10).map(|i| (i.to_string(), "t".to_owned())).collect();
        assert_eq!(fit_chars(&small).len(), 10);
        assert!(fit_chars(&[]).is_empty());
    }
}
