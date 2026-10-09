//! `ingest reembed [--yes] [--clear]`: make the database hold the configured
//! embedder's space (`docs/PROVIDERS.md` §4.3). When it holds
//! another, inside one transaction (`crate::db::space::switch_space`)
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
//! what would be embedded, its estimated cost at the configured price and what
//! is left under the spend cap, and exits non-zero having changed nothing. The
//! run is billed to the cap like every embedding (`judge_embed::MeteredEmbedder`),
//! so a re-embed larger than what is left stops at the cap with the rows so
//! far kept; the dry run says so beforehand, and how high to set
//! `JUDGE_MAX_USD` for the run. Before anything is cleared the configured embedder is probed with
//! one short text (a few tokens): a wrong key, URL or model name, or a model
//! whose real width is not the configured `dimensions`, fails here with the
//! old vectors intact — after the switch the only ways back would be paying
//! for the old space again or the R2 restore drill.

use std::fmt::Write as _;

use anyhow::Context as _;
use judge_core::InputKind;
use judge_embed::{EmbedPrice, Space, WithSpace};
use sqlx::{PgPool, Row as _};

use super::RefreshLease;
use crate::db::space::{VECTOR_TABLES, column_width, stored_counts, stored_space, switch_space};

/// Characters per token for the estimate: typical of English prose. The cap
/// reserves at one token per byte, so the estimate is what a run is likely
/// to cost and the worst case is what the cap can take for it.
const BYTES_PER_TOKEN: f64 = 4.0;
/// What the probe embeds. Short, so it costs next to nothing anywhere.
const PROBE_TEXT: &str = "Lifelink";

/// `(table, rows, bytes)` the embed loop would send: the UTF-8 bytes of
/// exactly the text each target in `embed.rs` selects (what the spend cap
/// reserves from), for every row after a switch, only the rows still empty
/// (`only_missing`) when the space is kept.
async fn workload(pool: &PgPool, only_missing: bool) -> anyhow::Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query(
        "SELECT 'rules' AS t, count(*) AS rows, coalesce(sum(octet_length(heading || E'\\n' || body || \
                CASE WHEN cardinality(examples) > 0 THEN E'\\nExample: ' || array_to_string(examples, E'\\nExample: ') ELSE '' END)), 0)::bigint AS bytes \
           FROM rules WHERE parent_id IS NULL AND (NOT $1 OR embedding IS NULL) \
         UNION ALL SELECT 'glossary', count(*), coalesce(sum(octet_length(term || E'\\n' || text)), 0)::bigint FROM glossary WHERE (NOT $1 OR embedding IS NULL) \
         UNION ALL SELECT 'calls', count(*), coalesce(sum(octet_length(question || E'\\n' || answer)), 0)::bigint FROM calls WHERE (NOT $1 OR embedding IS NULL)",
    )
    .bind(only_missing)
    .fetch_all(pool)
    .await
    .context("counting the reembed workload")?;
    rows.iter()
        .map(|r| Ok((r.try_get("t")?, r.try_get("rows")?, r.try_get("bytes")?)))
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
    let (mut rows, mut bytes) = (0i64, 0i64);
    for (table, n, b) in workload {
        let _ = writeln!(out, "  {table:<9} {n:>7} rows  {b:>10} bytes");
        rows += n;
        bytes += b;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "an estimate, printed to two decimals"
    )]
    let tokens = bytes as f64 / BYTES_PER_TOKEN;
    let _ = writeln!(
        out,
        "  total     {rows:>7} rows  {bytes:>10} bytes  ~{tokens:.0} tokens"
    );
    out
}

/// The most the embed loop can be charged for `rows` texts of `bytes` in
/// all at `rate` USD per million tokens: what the spend cap reserves for
/// them (`judge_embed::worst_case_tokens`, summed).
fn worst_usd(rows: i64, bytes: i64, rate: f64) -> f64 {
    let tokens = u64::try_from(bytes).unwrap_or(0).saturating_add(
        u64::try_from(rows)
            .unwrap_or(0)
            .saturating_mul(judge_embed::metered::TOKENS_PER_TEXT),
    );
    judge_embed::metered::usd_for(tokens, rate)
}

/// The `JUDGE_MAX_USD` that leaves room for `worst` on top of what is spent.
fn cap_for(worst: f64, room: Room) -> f64 {
    (room.cap - room.remaining + worst).ceil().max(1.0)
}

/// What the spend cap has room for when the dry run is printed.
#[derive(Clone, Copy, Debug)]
struct Room {
    /// USD left under the cap (`JUDGE_MAX_USD`, less what the period has spent).
    remaining: f64,
    /// The cap.
    cap: f64,
}

/// The cost lines of the dry run: the estimate at `price` (four bytes a
/// token), the worst case the cap may reserve ([`worst_usd`]), and what is
/// left under the cap, with the `JUDGE_MAX_USD` that fits the worst case
/// when it does not. Pure, so it can be read in a test.
fn cost(rows: i64, bytes: i64, price: EmbedPrice, room: Room) -> String {
    let Some(rate) = price.per_million() else {
        return "cost: nothing (the provider is pricing = \"free\"); calls are still counted\n"
            .to_owned();
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "an estimate, printed to two decimals"
    )]
    let likely = bytes as f64 / BYTES_PER_TOKEN / 1e6 * rate;
    let worst = worst_usd(rows, bytes, rate);
    let mut out = format!(
        "estimated cost: ~${likely:.2} at ${rate}/M tokens (at most ${worst:.2}, what the spend cap reserves)\n\
         spend cap: ${:.2} left of ${:.2} (JUDGE_MAX_USD)\n",
        room.remaining, room.cap
    );
    if worst > room.remaining {
        let _ = writeln!(
            out,
            "that may not fit: --yes refuses to clear anything unless it does (a refill of rows already \
             empty stops at the cap instead, keeping what it embedded); run it with JUDGE_MAX_USD={:.2} or more",
            cap_for(worst, room)
        );
    }
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
    lease: &mut RefreshLease,
    embedder: Option<&dyn WithSpace>,
    yes: bool,
    clear: bool,
) -> anyhow::Result<()> {
    let pool = lease.pool();
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
    let (rows, bytes) = work.iter().fold((0, 0), |(r, c), (_, n, b)| (r + n, c + b));
    let meter = embedder.meter();
    print!(
        "{}",
        cost(
            rows,
            bytes,
            embedder.price(),
            Room {
                remaining: meter.remaining_usd(),
                cap: meter.max_spend_usd(),
            },
        )
    );
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
    if !yes {
        let then = match (switching, same) {
            (true, true) => "clear and re-embed every row",
            (true, false) => "switch and re-embed",
            (false, _) if rows == 0 => "do nothing: no row is empty",
            (false, _) => "embed the rows still empty",
        };
        anyhow::bail!("reembed: dry run, nothing changed; rerun with --yes to {then}");
    }
    // A switch clears every vector before the refill pays for them again: one
    // the cap cannot see through would leave the vectors gone and too many
    // empty rows for the scheduled refresh to fill. So it must fit first.
    if switching && let Some(rate) = embedder.price().per_million() {
        let worst = worst_usd(rows, bytes, rate);
        let room = Room {
            remaining: meter.remaining_usd(),
            cap: meter.max_spend_usd(),
        };
        if worst > room.remaining {
            anyhow::bail!(
                "reembed: the re-embed may cost up to ${worst:.2} and the spend cap has ${:.2} left, so nothing was cleared; \
                 run it with JUDGE_MAX_USD={:.2} or more for this run \
                 (`docker compose run --rm -e JUDGE_MAX_USD={:.2} refresh reembed --yes`)",
                room.remaining,
                cap_for(worst, room),
                cap_for(worst, room)
            );
        }
    }
    if switching && same {
        switch_space(pool, target)
            .await
            .context("clearing the embedding space")?;
        println!(
            "cleared every vector in {target}, indexes rebuilt; the row did not change, so running processes log no mismatch \
             and their vector search answers from nothing until the refill finishes"
        );
    } else if switching {
        switch_space(pool, target)
            .await
            .context("switching the embedding space")?;
        println!(
            "switched to {target}: columns retyped, indexes rebuilt, vectors cleared; running processes re-read the space on their next request"
        );
    }
    let counts = super::embed::run(lease, Some(embedder)).await?;
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
    use judge_embed::Provider;
    use pgvector::Vector;

    use super::*;
    use crate::db::space::{column_width, record_space};
    use crate::ingest::embed::fake::{FakeExt as _, fake_embedder};

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
        assert!(!p.contains("cost"), "{p}");
        let room = Room {
            remaining: 5.0,
            cap: 5.0,
        };
        // 4M characters at $0.06/M: ~1M tokens, ~$0.06; at most 4M tokens and the per-text allowance.
        let c = cost(1910, 4_000_000, EmbedPrice::Table(0.06), room);
        assert!((worst_usd(1910, 4_000_000, 0.06) - 0.241_833_6).abs() < 1e-9);
        assert!(
            c.contains("estimated cost: ~$0.06 at $0.06/M tokens (at most $0.24")
                && c.contains("spend cap: $5.00 left of $5.00")
                && !c.contains("may stop"),
            "{c}"
        );
        // Less left than the worst case: the dry run names a cap that fits it.
        let tight = cost(
            1910,
            4_000_000,
            EmbedPrice::Table(0.18),
            Room {
                remaining: 0.30,
                cap: 5.0,
            },
        );
        assert!(
            tight.contains("may not fit") && tight.contains("JUDGE_MAX_USD=6.00"),
            "{tight}"
        );
        assert!(cost(1, 1, EmbedPrice::Free, room).contains("nothing"));
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
        let mut lease = crate::ingest::lease(&pool, "test").await?;
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
        let wrong = fake_embedder(Provider::OpenAi, "nomic-embed-text", 768).replying(1024);
        let err = run(&mut lease, Some(&wrong), true, false)
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

        // A switch the spend cap cannot see through is refused before anything
        // is cleared: probed ($10/M: 24 tokens reserved, 1 billed), then refused,
        // since the one glossary row's worst case (35 tokens, $0.00035) passes
        // the $0.00029 left.
        let meter = judge_llm::SpendMeter::new().with_max_spend_usd(0.0003)?;
        let capped = crate::ingest::embed::fake::billed(
            Provider::OpenAi,
            "nomic-embed-text",
            768,
            &meter,
            EmbedPrice::Table(10.0),
        );
        let err = run(&mut lease, Some(&capped), true, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("nothing was cleared") && err.contains("JUDGE_MAX_USD=1.00"),
            "{err}"
        );
        assert_eq!(capped.calls(), 1, "the probe only");
        unchanged(&pool).await?;

        // The dry run probes (so the endpoint is known to work) and stops.
        let nomic = fake_embedder(Provider::OpenAi, "nomic-embed-text", 768);
        let err = run(&mut lease, Some(&nomic), false, false)
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
        run(&mut lease, Some(&nomic), true, false).await?;
        assert_eq!(stored_space(&pool).await?, Some(nomic.space().clone()));
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
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        let nomic = fake_embedder(Provider::OpenAi, "nomic-embed-text", 768);
        // The database already holds nomic's space: one embedded row (a marker
        // the fake would never produce) and one still empty, as an interrupted
        // refill leaves things.
        switch_space(&pool, nomic.space()).await?;
        let marker = vec![0.25_f32; 768];
        sqlx::query("INSERT INTO glossary (term, text, cr_version, embedding) VALUES ('Lifelink', 'A keyword.', '20260819', $1)")
            .bind(Vector::from(marker.clone()))
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO glossary (term, text, cr_version) VALUES ('Trample', 'Another.', '20260819')").execute(&pool).await?;

        // Dry run says so, and what it would embed is the one empty row.
        let err = run(&mut lease, Some(&nomic), false, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("embed the rows still empty"), "{err}");
        assert!(holds(&pool, "Lifelink", &marker).await?);

        // --yes without --clear: no switch, the marker survives, the empty row is filled.
        run(&mut lease, Some(&nomic), true, false).await?;
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
        run(&mut lease, Some(&nomic), true, false).await?;
        assert_eq!(nomic.calls(), 4, "the probe only");
        let err = run(&mut lease, Some(&nomic), false, false)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("do nothing: no row is empty"), "{err}");
        // --clear without --yes is a dry run that says what it would clear.
        let err = run(&mut lease, Some(&nomic), false, true)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("clear and re-embed every row"), "{err}");
        assert!(holds(&pool, "Lifelink", &marker).await?);

        // --clear: the switch clears everything and both rows are re-embedded.
        run(&mut lease, Some(&nomic), true, true).await?;
        assert!(
            holds(&pool, "Lifelink", &vec![0.5; 768]).await?,
            "--clear did not clear the stored vector"
        );
        assert!(holds(&pool, "Trample", &vec![0.5; 768]).await?);
        assert_eq!(stored_space(&pool).await?, Some(nomic.space().clone()));

        // A row edited by hand to name a width the columns do not have is not
        // "already held": --yes switches (retypes) rather than refilling into
        // a refusal.
        sqlx::query("UPDATE embedding_space SET dimensions = 1536")
            .execute(&pool)
            .await?;
        let wide = fake_embedder(Provider::OpenAi, "nomic-embed-text", 1536);
        run(&mut lease, Some(&wide), true, false).await?;
        assert_eq!(column_width(&pool, "glossary").await?, 1536);
        assert!(holds(&pool, "Trample", &vec![0.5; 1536]).await?);
        Ok(())
    }
}
