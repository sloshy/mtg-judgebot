//! `ingest reembed [--yes]`: move the database to the configured embedder's
//! space (`docs/proposals/providers.md` §4.3). Inside one transaction
//! (`judge_bot::db::space::switch_space`) every `embedding` column is retyped
//! to the new width, its HNSW index recreated as the migrations define it and
//! every vector cleared, and `embedding_space` rewritten; then the ordinary
//! embed loop refills every row. The switch is all or nothing; the refill is
//! resumable (`ingest embed` fills whatever is still NULL).
//!
//! Re-embedding pays the provider for every row, which is why the command is
//! a dry run without `--yes`: it prints what is stored and would be cleared,
//! what would be embedded and a rough cost, and exits non-zero having changed
//! nothing. Before anything is cleared the configured embedder is probed with
//! one short text (a few tokens): a wrong key, URL or model name, or a model
//! whose real width is not the configured `dimensions`, fails here with the
//! old vectors intact — after the switch the only ways back would be paying
//! for the old space again or the R2 restore drill.

use std::fmt::Write as _;

use anyhow::Context as _;
use judge_bot::db::space::{stored_counts, stored_space, switch_space};
use judge_core::InputKind;
use judge_embed::{Space, WithSpace};
use sqlx::{PgPool, Row as _};

/// A rough all-in price per million tokens for the estimate below: the top
/// of the common range (`text-embedding-3-large` $0.13, `voyage-3-large`
/// $0.18 — `voyage-3.5` and `text-embedding-3-small` are a third of that or
/// less, a local model is free). Chars-to-tokens at 4:1. It is an order of
/// magnitude, not a quote.
const ROUGH_USD_PER_MILLION_TOKENS: f64 = 0.15;
const CHARS_PER_TOKEN: f64 = 4.0;
/// What the probe embeds. Short, so it costs next to nothing anywhere.
const PROBE_TEXT: &str = "Lifelink";

/// `(table, rows, characters)` the embed loop would send: the same text each
/// target in `embed.rs` selects.
async fn workload(pool: &PgPool) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query(
        "SELECT 'rules' AS t, count(*) AS rows, coalesce(sum(length(heading) + length(body) + length(array_to_string(examples, ' '))), 0)::bigint AS chars \
           FROM rules WHERE parent_id IS NULL \
         UNION ALL SELECT 'glossary', count(*), coalesce(sum(length(term) + length(text)), 0)::bigint FROM glossary \
         UNION ALL SELECT 'calls', count(*), coalesce(sum(length(question) + length(answer)), 0)::bigint FROM calls",
    )
    .fetch_all(pool)
    .await
    .context("counting the reembed workload")?;
    rows.iter().map(|r| Ok((r.try_get("t")?, r.try_get("rows")?, r.try_get("chars")?))).collect()
}

/// The dry-run report: what is stored and would be cleared, what is
/// configured, the rows and a rough cost. Pure over the counts so it can be
/// read in a test.
fn plan(stored: Option<&Space>, held: &[(&str, i64)], target: &Space, workload: &[(String, i64, i64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "embedding space: {} -> {target}", stored.map_or_else(|| "none (nothing embedded)".to_owned(), ToString::to_string));
    let vectors: i64 = held.iter().map(|(_, n)| n).sum();
    let breakdown = held.iter().map(|(t, n)| format!("{t} {n}")).collect::<Vec<_>>().join(", ");
    let _ = writeln!(out, "clearing {vectors} stored vectors ({breakdown})");
    let (mut rows, mut chars) = (0i64, 0i64);
    for (table, n, c) in workload {
        let _ = writeln!(out, "  {table:<9} {n:>7} rows  {c:>10} chars");
        rows += n;
        chars += c;
    }
    #[expect(clippy::cast_precision_loss, reason = "an estimate, printed to two decimals")]
    let tokens = chars as f64 / CHARS_PER_TOKEN;
    let usd = tokens / 1e6 * ROUGH_USD_PER_MILLION_TOKENS;
    let _ = writeln!(out, "  total     {rows:>7} rows  {chars:>10} chars  ~{tokens:.0} tokens");
    let _ = writeln!(
        out,
        "rough cost: ~${usd:.2} at ${ROUGH_USD_PER_MILLION_TOKENS}/M tokens (an order of magnitude: check your provider's price; a local model is free)"
    );
    out
}

/// One request against the configured embedder, before anything is cleared:
/// it must answer with one vector of the target width.
///
/// # Errors
/// The embedder's own error (bad key, URL, model), or a width that is not
/// the configured `dimensions`.
async fn probe(embedder: &dyn WithSpace) -> anyhow::Result<usize> {
    let target = embedder.space();
    let mut vectors = embedder
        .embed(&[PROBE_TEXT], InputKind::Document)
        .await
        .map_err(|e| anyhow::anyhow!("probing {target}: {e}"))?;
    let Some(v) = vectors.pop().filter(|_| vectors.is_empty()) else {
        anyhow::bail!("probing {target}: expected one vector for one text");
    };
    if v.len() != target.dimensions {
        anyhow::bail!(
            "probing {target}: the model answered with a {}-dimensional vector, not {}; set models.embed.dimensions to what the model produces (with send_dimensions = false the server chooses)",
            v.len(),
            target.dimensions
        );
    }
    Ok(v.len())
}

/// Print the plan, probe the embedder and, with `yes`, switch the database
/// and embed everything.
///
/// # Errors
/// Without `yes` (nothing changed), without an embedder, when the probe
/// fails (nothing changed), or when the switch or the embed loop fails. A
/// failed switch leaves the database as it was.
pub async fn run(pool: &PgPool, embedder: Option<&dyn WithSpace>, yes: bool) -> anyhow::Result<()> {
    let Some(embedder) = embedder else {
        anyhow::bail!("reembed: no embedder configured (set VOYAGE_API_KEY or [models.embed])");
    };
    let target = embedder.space();
    let stored = stored_space(pool).await?;
    let held = stored_counts(pool).await?;
    let work = workload(pool).await?;
    print!("{}", plan(stored.as_ref(), &held, target, &work));
    if stored.as_ref() == Some(target) {
        println!("note: the database already holds {target}; this clears and re-embeds every vector in the same space, paying for every row again");
    }
    let width = probe(embedder).await?;
    println!("probe: {target} answered with a {width}-dimensional vector");
    if !yes {
        anyhow::bail!("reembed: dry run, nothing changed; rerun with --yes to switch and re-embed");
    }
    switch_space(pool, target).await.context("switching the embedding space")?;
    println!("switched to {target}: columns retyped, indexes rebuilt, vectors cleared; running processes re-read the space on their next request");
    super::embed::run(pool, Some(embedder)).await
}

#[cfg(test)]
mod tests {
    use judge_bot::db::space::{column_width, record_space};
    use judge_embed::Provider;
    use pgvector::Vector;

    use super::*;
    use crate::embed::fake::Fake;

    #[test]
    fn the_plan_names_both_spaces_the_rows_and_says_the_cost_is_rough() {
        let target = Space { provider: Provider::OpenAi, model: "nomic-embed-text".into(), dimensions: 768 };
        let stored = Space { provider: Provider::Voyage, model: "voyage-3.5".into(), dimensions: 1024 };
        let held = [("rules", 1173), ("glossary", 700), ("calls", 9)];
        let work = vec![("rules".to_owned(), 1200, 4_000_000), ("glossary".to_owned(), 700, 100_000), ("calls".to_owned(), 10, 20_000)];
        let p = plan(Some(&stored), &held, &target, &work);
        assert!(p.contains("voyage/voyage-3.5 (1024 dims) -> openai/nomic-embed-text (768 dims)"), "{p}");
        assert!(p.contains("clearing 1882 stored vectors (rules 1173, glossary 700, calls 9)"), "{p}");
        assert!(p.contains("rules        1200 rows") && p.contains("total        1910 rows") && p.contains("~1030000 tokens"), "{p}");
        assert!(p.contains("rough cost: ~$0.15"), "{p}");
        assert!(plan(None, &[], &target, &[]).contains("none (nothing embedded) -> openai"));
    }

    async fn stored_glossary(pool: &PgPool) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO glossary (term, text, cr_version, embedding) VALUES ('Lifelink', 'A keyword.', '20260819', $1)")
            .bind(Vector::from(vec![0.1; 1024]))
            .execute(pool)
            .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_failed_probe_or_a_dry_run_changes_nothing_and_yes_switches_then_embeds(pool: PgPool) -> anyhow::Result<()> {
        let voyage = Space { provider: Provider::Voyage, model: "voyage-3.5".into(), dimensions: 1024 };
        record_space(&pool, &voyage).await?;
        stored_glossary(&pool).await?;
        let before = stored_counts(&pool).await?;
        let unchanged = |pool: &PgPool| {
            let before = before.clone();
            let voyage = voyage.clone();
            let pool = pool.clone();
            async move {
                assert_eq!(stored_space(&pool).await?, Some(voyage));
                assert_eq!(stored_counts(&pool).await?, before);
                assert_eq!(column_width(&pool, "glossary").await?, 1024);
                anyhow::Ok(())
            }
        };

        // A model that does not produce the configured width: refused with --yes, before the switch.
        let wrong = Fake::new(Provider::OpenAi, "nomic-embed-text", 768).replying(1024);
        let err = run(&pool, Some(&wrong), true).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(err.contains("probing openai/nomic-embed-text (768 dims)") && err.contains("1024-dimensional vector, not 768"), "{err}");
        assert_eq!(wrong.calls(), 1);
        unchanged(&pool).await?;

        // The dry run probes (so the endpoint is known to work) and stops.
        let nomic = Fake::new(Provider::OpenAi, "nomic-embed-text", 768);
        let err = run(&pool, Some(&nomic), false).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(err.contains("dry run, nothing changed"), "{err}");
        assert_eq!(nomic.calls(), 1);
        unchanged(&pool).await?;

        // --yes: the switch, then the embed loop over every row.
        run(&pool, Some(&nomic), true).await?;
        assert_eq!(stored_space(&pool).await?, Some(nomic.space.clone()));
        assert_eq!(column_width(&pool, "glossary").await?, 768);
        assert_eq!(stored_counts(&pool).await?.iter().find(|(t, _)| *t == "glossary").map(|(_, n)| *n), Some(1));
        assert_eq!(nomic.calls(), 3, "the dry run's probe, this run's probe and one glossary batch");
        Ok(())
    }
}
