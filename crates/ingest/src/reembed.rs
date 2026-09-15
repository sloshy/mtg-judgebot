//! `ingest reembed [--yes] [--clear]`: make the database hold the configured
//! embedder's space (`docs/proposals/providers.md` §4.3). When it holds
//! another, inside one transaction (`judge_bot::db::space::switch_space`)
//! every `embedding` column is retyped to the new width, its HNSW index
//! recreated as the migrations define it and every vector cleared, and
//! `embedding_space` rewritten; then the ordinary embed loop refills every
//! row. The switch is all or nothing; the refill is resumable.
//!
//! When the database already holds the configured space there is nothing to
//! switch, and the command is idempotent: it runs only the refill, so
//! re-running it after an interrupted refill embeds what is still empty and
//! pays for nothing twice. `--clear` is the deliberate exception — clear and
//! re-embed every vector in the same space (a provider that changed the
//! model behind a name) — and costs every row.
//!
//! Re-embedding pays the provider per row, which is why the command is a dry
//! run without `--yes`: it prints what is stored and what would happen to it,
//! what would be embedded and a rough cost, and exits non-zero having changed
//! nothing. Before anything is cleared the configured embedder is probed with
//! one short text (a few tokens): a wrong key, URL or model name, or a model
//! whose real width is not the configured `dimensions`, fails here with the
//! old vectors intact — after the switch the only ways back would be paying
//! for the old space again or the R2 restore drill.

use std::fmt::Write as _;

use anyhow::Context as _;
use judge_bot::db::space::{
    VECTOR_TABLES, column_width, stored_counts, stored_space, switch_space,
};
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
/// target in `embed.rs` selects — every row after a switch, only the rows
/// still empty (`only_missing`) when the space is kept.
async fn workload(pool: &PgPool, only_missing: bool) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query(
        "SELECT 'rules' AS t, count(*) AS rows, coalesce(sum(length(heading) + length(body) + length(array_to_string(examples, ' '))), 0)::bigint AS chars \
           FROM rules WHERE parent_id IS NULL AND (NOT $1 OR embedding IS NULL) \
         UNION ALL SELECT 'glossary', count(*), coalesce(sum(length(term) + length(text)), 0)::bigint FROM glossary WHERE (NOT $1 OR embedding IS NULL) \
         UNION ALL SELECT 'calls', count(*), coalesce(sum(length(question) + length(answer)), 0)::bigint FROM calls WHERE (NOT $1 OR embedding IS NULL)",
    )
    .bind(only_missing)
    .fetch_all(pool)
    .await
    .context("counting the reembed workload")?;
    rows.iter()
        .map(|r| Ok((r.try_get("t")?, r.try_get("rows")?, r.try_get("chars")?)))
        .collect()
}

/// The dry-run report: what is stored and what happens to it (cleared by a
/// switch, kept otherwise), what is configured, the rows and a rough cost.
/// Pure over the counts so it can be read in a test.
fn plan(
    stored: Option<&Space>,
    held: &[(&str, i64)],
    target: &Space,
    workload: &[(String, i64, i64)],
    switching: bool,
) -> String {
    let mut out = String::new();
    let vectors: i64 = held.iter().map(|(_, n)| n).sum();
    let breakdown = held
        .iter()
        .map(|(t, n)| format!("{t} {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    if switching {
        let from = match stored {
            Some(s) if s == target => format!("{target}, already held (kept)"),
            Some(s) => format!("{s} -> {target}"),
            None if vectors > 0 => format!("none ({vectors} unlabelled vectors) -> {target}"),
            None => format!("none (nothing embedded) -> {target}"),
        };
        let _ = writeln!(out, "embedding space: {from}");
        let _ = writeln!(out, "clearing {vectors} stored vectors ({breakdown})");
    } else {
        let _ = writeln!(
            out,
            "embedding space: {target}, already held; nothing to switch"
        );
        let _ = writeln!(
            out,
            "keeping {vectors} stored vectors ({breakdown}); embedding only rows still empty"
        );
    }
    let (mut rows, mut chars) = (0i64, 0i64);
    for (table, n, c) in workload {
        let _ = writeln!(out, "  {table:<9} {n:>7} rows  {c:>10} chars");
        rows += n;
        chars += c;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "an estimate, printed to two decimals"
    )]
    let tokens = chars as f64 / CHARS_PER_TOKEN;
    let usd = tokens / 1e6 * ROUGH_USD_PER_MILLION_TOKENS;
    let _ = writeln!(
        out,
        "  total     {rows:>7} rows  {chars:>10} chars  ~{tokens:.0} tokens"
    );
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
/// when it holds another space (or `clear` says to clear this one), then
/// embed every row still empty.
///
/// # Errors
/// Without `yes` (nothing changed), without an embedder, when the probe
/// fails (nothing changed), or when the switch or the embed loop fails. A
/// failed switch leaves the database as it was.
pub async fn run(
    pool: &PgPool,
    embedder: Option<&dyn WithSpace>,
    yes: bool,
    clear: bool,
) -> anyhow::Result<()> {
    let Some(embedder) = embedder else {
        anyhow::bail!("reembed: no embedder configured (set VOYAGE_API_KEY or [models.embed])");
    };
    let target = embedder.space();
    let stored = stored_space(pool).await?;
    let held = stored_counts(pool).await?;
    // "Already held" means the row *and* the columns: a row edited by hand to
    // match while a column still has the old width would otherwise send the
    // operator round in a circle (the refill refuses the width and says to
    // reembed; reembed sees the row and refills).
    let mut widths_match = true;
    for t in VECTOR_TABLES {
        widths_match &= column_width(pool, t.table).await? == target.dimensions;
    }
    let same = stored.as_ref() == Some(target) && widths_match;
    let switching = !same || clear;
    let work = workload(pool, !switching).await?;
    print!("{}", plan(stored.as_ref(), &held, target, &work, switching));
    if same && clear {
        let verb = if yes { "clearing" } else { "would clear" };
        println!(
            "--clear: {verb} and re-embed every vector in {target}, paying for every row a second time"
        );
    } else if same {
        println!(
            "(--clear would clear and re-embed every vector in the same space, paying for every row again)"
        );
    } else if clear {
        println!("(--clear is redundant: the space is changing anyway, which clears every vector)");
    }
    if let Some(s) = &stored
        && !same
        && s != target
        && s.provider == target.provider
        && s.dimensions == target.dimensions
    {
        println!(
            "only the model name differs ({} -> {}): if the stored vectors were in fact produced by {}, \
             `UPDATE embedding_space SET model = '{}'` relabels them for free; --yes re-embeds every row",
            s.model, target.model, target.model, target.model
        );
    }
    let width = probe(embedder).await?;
    println!("probe: {target} answered with a {width}-dimensional vector");
    let rows: i64 = work.iter().map(|(_, n, _)| n).sum();
    if !yes {
        let then = match (switching, same) {
            (true, true) => "clear and re-embed every row",
            (true, false) => "switch and re-embed",
            (false, _) if rows == 0 => "do nothing: no row is empty",
            (false, _) => "embed the rows still empty",
        };
        anyhow::bail!("reembed: dry run, nothing changed; rerun with --yes to {then}");
    }
    if switching && same {
        switch_space(pool, target)
            .await
            .context("clearing the embedding space")?;
        println!(
            "cleared every vector in {target}, indexes rebuilt; the row did not change, so running processes log no mismatch \
             and their vector legs answer from nothing until the refill finishes"
        );
    } else if switching {
        switch_space(pool, target)
            .await
            .context("switching the embedding space")?;
        println!(
            "switched to {target}: columns retyped, indexes rebuilt, vectors cleared; running processes re-read the space on their next request"
        );
    }
    let counts = super::embed::run(pool, Some(embedder)).await?;
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    let breakdown = counts
        .iter()
        .map(|(t, n)| format!("{t} {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("embedded {total} rows ({breakdown}); no row is empty");
    Ok(())
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
        let target = Space {
            provider: Provider::OpenAi,
            model: "nomic-embed-text".into(),
            dimensions: 768,
        };
        let stored = Space {
            provider: Provider::Voyage,
            model: "voyage-3.5".into(),
            dimensions: 1024,
        };
        let held = [("rules", 1173), ("glossary", 700), ("calls", 9)];
        let work = vec![
            ("rules".to_owned(), 1200, 4_000_000),
            ("glossary".to_owned(), 700, 100_000),
            ("calls".to_owned(), 10, 20_000),
        ];
        let p = plan(Some(&stored), &held, &target, &work, true);
        assert!(
            p.contains("voyage/voyage-3.5 (1024 dims) -> openai/nomic-embed-text (768 dims)"),
            "{p}"
        );
        assert!(
            p.contains("clearing 1882 stored vectors (rules 1173, glossary 700, calls 9)"),
            "{p}"
        );
        assert!(
            p.contains("rules        1200 rows")
                && p.contains("total        1910 rows")
                && p.contains("~1030000 tokens"),
            "{p}"
        );
        assert!(p.contains("rough cost: ~$0.15"), "{p}");
        assert!(plan(None, &[], &target, &[], true).contains("none (nothing embedded) -> openai"));
        assert!(
            plan(None, &held, &target, &[], true)
                .contains("none (1882 unlabelled vectors) -> openai")
        );
        let cleared = plan(Some(&target), &held, &target, &[], true);
        assert!(
            cleared.contains("openai/nomic-embed-text (768 dims), already held (kept)")
                && cleared.contains("clearing 1882"),
            "{cleared}"
        );
        // Same space, no switch: what is held is kept, and the rows listed are the empty ones.
        let kept = plan(
            Some(&target),
            &held,
            &target,
            &[("glossary".to_owned(), 3, 300)],
            false,
        );
        assert!(
            kept.contains("openai/nomic-embed-text (768 dims), already held; nothing to switch"),
            "{kept}"
        );
        assert!(kept.contains("keeping 1882 stored vectors (rules 1173, glossary 700, calls 9); embedding only rows still empty"), "{kept}");
        assert!(
            kept.contains("total           3 rows") && !kept.contains("clearing"),
            "{kept}"
        );
    }

    async fn stored_glossary(pool: &PgPool) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO glossary (term, text, cr_version, embedding) VALUES ('Lifelink', 'A keyword.', '20260819', $1)")
            .bind(Vector::from(vec![0.1; 1024]))
            .execute(pool)
            .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_failed_probe_or_a_dry_run_changes_nothing_and_yes_switches_then_embeds(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let voyage = Space {
            provider: Provider::Voyage,
            model: "voyage-3.5".into(),
            dimensions: 1024,
        };
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
        let err = run(&pool, Some(&wrong), true, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("probing openai/nomic-embed-text (768 dims)")
                && err.contains("1024-dimensional vector, not 768"),
            "{err}"
        );
        assert_eq!(wrong.calls(), 1);
        unchanged(&pool).await?;

        // The dry run probes (so the endpoint is known to work) and stops.
        let nomic = Fake::new(Provider::OpenAi, "nomic-embed-text", 768);
        let err = run(&pool, Some(&nomic), false, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("dry run, nothing changed") && err.contains("switch and re-embed"),
            "{err}"
        );
        assert_eq!(nomic.calls(), 1);
        unchanged(&pool).await?;

        // --yes: the switch, then the embed loop over every row.
        run(&pool, Some(&nomic), true, false).await?;
        assert_eq!(stored_space(&pool).await?, Some(nomic.space.clone()));
        assert_eq!(column_width(&pool, "glossary").await?, 768);
        assert_eq!(
            stored_counts(&pool)
                .await?
                .iter()
                .find(|(t, _)| *t == "glossary")
                .map(|(_, n)| *n),
            Some(1)
        );
        assert_eq!(
            nomic.calls(),
            3,
            "the dry run's probe, this run's probe and one glossary batch"
        );
        Ok(())
    }

    /// The vector `row` holds, compared as pgvector does.
    async fn holds(pool: &PgPool, term: &str, v: &[f32]) -> anyhow::Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT embedding = $1 FROM glossary WHERE term = $2")
                .bind(Vector::from(v.to_vec()))
                .bind(term)
                .fetch_one(pool)
                .await?,
        )
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn the_same_space_is_a_refill_not_a_switch_unless_clear(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let nomic = Fake::new(Provider::OpenAi, "nomic-embed-text", 768);
        // The database already holds nomic's space: one embedded row (a marker
        // the fake would never produce) and one still empty, as an interrupted
        // refill leaves things.
        switch_space(&pool, &nomic.space).await?;
        let marker = vec![0.25_f32; 768];
        sqlx::query("INSERT INTO glossary (term, text, cr_version, embedding) VALUES ('Lifelink', 'A keyword.', '20260819', $1)")
            .bind(Vector::from(marker.clone()))
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO glossary (term, text, cr_version) VALUES ('Trample', 'Another.', '20260819')").execute(&pool).await?;

        // Dry run says so, and what it would embed is the one empty row.
        let err = run(&pool, Some(&nomic), false, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("embed the rows still empty"), "{err}");
        assert!(holds(&pool, "Lifelink", &marker).await?);

        // --yes without --clear: no switch, the marker survives, the empty row is filled.
        run(&pool, Some(&nomic), true, false).await?;
        assert!(
            holds(&pool, "Lifelink", &marker).await?,
            "the stored vector was re-embedded"
        );
        assert!(holds(&pool, "Trample", &vec![0.5; 768]).await?);
        assert_eq!(
            nomic.calls(),
            3,
            "two probes and one batch of the single empty row"
        );
        // Idempotent: nothing left to embed, nothing paid; the dry run says so too.
        run(&pool, Some(&nomic), true, false).await?;
        assert_eq!(nomic.calls(), 4, "the probe only");
        let err = run(&pool, Some(&nomic), false, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("do nothing: no row is empty"), "{err}");
        // --clear without --yes is a dry run that says what it would clear.
        let err = run(&pool, Some(&nomic), false, true)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("clear and re-embed every row"), "{err}");
        assert!(holds(&pool, "Lifelink", &marker).await?);

        // --clear: the switch clears everything and both rows are re-embedded.
        run(&pool, Some(&nomic), true, true).await?;
        assert!(
            holds(&pool, "Lifelink", &vec![0.5; 768]).await?,
            "--clear did not clear the stored vector"
        );
        assert!(holds(&pool, "Trample", &vec![0.5; 768]).await?);
        assert_eq!(stored_space(&pool).await?, Some(nomic.space.clone()));

        // A row edited by hand to name a width the columns do not have is not
        // "already held": --yes switches (retypes) rather than refilling into
        // a refusal.
        sqlx::query("UPDATE embedding_space SET dimensions = 1536")
            .execute(&pool)
            .await?;
        let wide = Fake::new(Provider::OpenAi, "nomic-embed-text", 1536);
        run(&pool, Some(&wide), true, false).await?;
        assert_eq!(column_width(&pool, "glossary").await?, 1536);
        assert!(holds(&pool, "Trample", &vec![0.5; 1536]).await?);
        Ok(())
    }
}
