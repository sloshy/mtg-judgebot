//! Fill NULL `embedding` columns on `rules`, `glossary` and `calls` via the `Embedder`.
//!
//! Only rule-level `rules` rows (`parent_id IS NULL`) are embedded: the retriever's
//! vector search returns rule chunks only (leaves are folded into their rule's body), and
//! the HNSW index is partial over exactly those rows.
//!
//! Each table is walked in batches of at most `MAX_BATCH` rows (Voyage's per-request
//! limit) whose `embedding IS NULL`; texts are embedded with `InputKind::Document` and
//! written back as `pgvector` vectors. Without an embedder nothing is touched.
//!
//! Before the first request the embedder's space is checked against the database
//! (`crate::db::space`): the columns' actual `vector(N)` must be the embedder's
//! width, and `embedding_space`, when present, must name the embedder's provider and
//! model — a different model is a refusal pointing at `ingest reembed`, because a
//! vector of another model in the same column is silently wrong at query time. Then
//! every batch re-reads the row inside its own transaction under the shared side of
//! `CALLS_REWRITE_LOCK` (`hold_space`): a `reembed` that commits while this loop
//! runs is seen by the next batch, which refuses instead of writing the old model's
//! vectors under the new row. The row is written by the first batch that writes a
//! vector, in the same transaction, so a run that fails before then leaves no label
//! behind it; vectors it did not write are never relabelled.
//!
//! Every request is behind the process's spend cap (the embedder is a
//! `judge_embed::MeteredEmbedder`): a batch the cap cannot fit is refused
//! before it is sent and the step fails naming the cap (`refused`), keeping
//! what it has embedded. The scheduled refresh reports that like any failed
//! step, to `JUDGE_ALERT_WEBHOOK`.

use anyhow::Context as _;
use judge_core::InputKind;
use judge_embed::{Space, WithSpace};
use pgvector::Vector;
use sqlx::{PgPool, Row as _};

use super::RefreshLease;
use crate::db::space::{
    VECTOR_TABLES, column_width, hold_space, record_space, stored_counts, stored_space,
};

/// Voyage accepts at most 128 texts per request.
const MAX_BATCH: usize = 128;
/// Free-tier Voyage allows only 10K tokens/request-minute; `VOYAGE_MAX_BATCH`
/// lets an ingest run shrink batches to fit (default: [`MAX_BATCH`]).
fn max_batch() -> usize {
    std::env::var("VOYAGE_MAX_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_BATCH)
}
/// Soft cap on characters per request so a batch of long rule chunks stays under the
/// provider's token budget (voyage-3.5: 320k tokens; ~4 chars/token, with margin).
const MAX_BATCH_CHARS: usize = 400_000;

/// The most rows a scheduled refresh embeds without a person deciding to:
/// above it the step is skipped ([`super::runs::Skip::EmbedCeiling`]) and the
/// operator alerted. Manual runs have no ceiling.
///
/// Sized against a full database (CR 20260819): 1,173 rule-level rules, 739
/// glossary terms and the stored calls, so a full re-embed is about 1,900
/// rows plus one per call. A new CR release empties only the rules whose text
/// changed, a few hundred, and a day adds a handful of calls. 800 is twice a
/// large CR release and well under half a full re-embed: more empty vectors
/// than that means something else emptied them (a `reembed --clear` that
/// died, an embedder configured on a database that never had one), and
/// paying for that is the operator's call, not a timer's.
pub const UNATTENDED_CEILING: u64 = 800;

/// One embeddable table: how to select unembedded rows and write vectors back.
/// Keys are selected as `text` so one loop serves `text` and `uuid` primary keys.
struct Target {
    name: &'static str,
    select: &'static str,
    /// How many rows `select` would walk, in all.
    count: &'static str,
    update: &'static str,
}

const TARGETS: [Target; 3] = [
    Target {
        name: "rules",
        select: "SELECT id::text AS key, heading || E'\\n' || body || \
                 CASE WHEN cardinality(examples) > 0 THEN E'\\nExample: ' || array_to_string(examples, E'\\nExample: ') ELSE '' END AS txt \
                 FROM rules WHERE embedding IS NULL AND parent_id IS NULL ORDER BY id LIMIT $1",
        count: "SELECT count(*) FROM rules WHERE embedding IS NULL AND parent_id IS NULL",
        update: "UPDATE rules SET embedding = $1 WHERE id = $2 AND embedding IS NULL",
    },
    Target {
        name: "glossary",
        select: "SELECT term AS key, term || E'\\n' || text AS txt FROM glossary WHERE embedding IS NULL ORDER BY term LIMIT $1",
        count: "SELECT count(*) FROM glossary WHERE embedding IS NULL",
        update: "UPDATE glossary SET embedding = $1 WHERE term = $2 AND embedding IS NULL",
    },
    Target {
        name: "calls",
        select: "SELECT id::text AS key, question || E'\\n' || answer AS txt FROM calls WHERE embedding IS NULL ORDER BY id LIMIT $1",
        count: "SELECT count(*) FROM calls WHERE embedding IS NULL",
        update: "UPDATE calls SET embedding = $1 WHERE id = $2::uuid AND embedding IS NULL",
    },
];

/// Embed every row whose `embedding IS NULL`. With no embedder configured this
/// logs a warning and does nothing.
///
/// # Errors
/// When the embedder's space is not the database's (see `check_space`), or on
/// embedding or database failure. Returns the rows embedded per table.
pub async fn run(
    lease: &mut RefreshLease,
    embedder: Option<&dyn WithSpace>,
) -> anyhow::Result<Vec<(&'static str, usize)>> {
    let pool = lease.pool();
    let Some(embedder) = embedder else {
        tracing::warn!(
            "ingest embed: skipped, no embedder configured (set VOYAGE_API_KEY or [models.embed])"
        );
        return Ok(Vec::new());
    };
    check_space(pool, embedder.space()).await?;
    let mut counts = Vec::with_capacity(TARGETS.len());
    for t in &TARGETS {
        let n = embed_table(pool, embedder, t)
            .await
            .with_context(|| format!("embedding {}", t.name))?;
        tracing::info!(table = t.name, embedded = n, "ingest embed");
        counts.push((t.name, n));
    }
    Ok(counts)
}

/// How many rows [`run`] would embed now, over every table: what a scheduled
/// run weighs against [`UNATTENDED_CEILING`] before it pays for any.
///
/// # Errors
/// On a database failure.
pub async fn pending(pool: &PgPool) -> anyhow::Result<u64> {
    let mut total = 0u64;
    for t in &TARGETS {
        let n: i64 = sqlx::query_scalar(t.count)
            .fetch_one(pool)
            .await
            .with_context(|| format!("counting unembedded {}", t.name))?;
        total = total.saturating_add(u64::try_from(n).unwrap_or_default());
    }
    Ok(total)
}

/// Refuse unless `space` can be the one the database holds: every `embedding`
/// column must be `vector(N)` at the space's width (the catalogue's typmod, not a
/// constant), and `embedding_space` must equal `space` or be absent while no table
/// holds a vector (vectors with no row came from somewhere this run cannot name).
/// The row itself is written by the first batch ([`embed_table`]), not here.
///
/// # Errors
/// A message naming both spaces or the offending column, and the way out:
/// configure the stored model, or `ingest reembed --yes`.
async fn check_space(pool: &PgPool, space: &Space) -> anyhow::Result<()> {
    let stored = stored_space(pool).await?;
    for t in VECTOR_TABLES {
        let width = column_width(pool, t.table).await?;
        if width != space.dimensions {
            let holds = stored.as_ref().map_or_else(String::new, |s| {
                format!(" (the database holds {s}: configure that model, or)")
            });
            anyhow::bail!(
                "{}.embedding is vector({width}) but the configured embedder {space} produces {}-dimensional vectors;{holds} \
                 run `ingest reembed --yes` to switch the database (re-embeds everything, which costs money)",
                t.table,
                space.dimensions
            );
        }
    }
    if let Some(stored) = stored {
        space.check(&stored)?;
    } else {
        let held: i64 = stored_counts(pool).await?.iter().map(|(_, n)| n).sum();
        if held > 0 {
            anyhow::bail!(
                "{held} vectors are stored but embedding_space is empty, so they cannot be attributed to {space}; \
                 INSERT the row naming what embedded them, or run `ingest reembed --yes`"
            );
        }
    }
    Ok(())
}

/// Embed `t` until no NULL rows remain; returns the number of rows embedded.
async fn embed_table(pool: &PgPool, embedder: &dyn WithSpace, t: &Target) -> anyhow::Result<usize> {
    let space = embedder.space();
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
            let txt = if txt.trim().is_empty() {
                key.clone()
            } else {
                txt
            };
            pairs.push((key, txt));
        }
        let batch = fit_chars(&pairs);
        let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = embedder
            .embed(&texts, InputKind::Document)
            .await
            .map_err(|e| refused(&e, total))?;
        if vectors.len() != batch.len() {
            anyhow::bail!(
                "embedder returned {} vectors for {} texts",
                vectors.len(),
                batch.len()
            );
        }
        let mut written = 0u64;
        let mut tx = pool.begin().await?;
        // Under the shared lock until commit: the row cannot change under this batch.
        if let Some(stored) = hold_space(&mut tx).await? {
            space.check(&stored).context(
                "the embedding space changed while embedding (a concurrent `ingest reembed`?)",
            )?;
        } else {
            // The first write establishes the space ([`check_space`] saw no vectors).
            record_space(&mut *tx, space).await?;
            tracing::info!(%space, "embedding_space recorded (first embed)");
        }
        let want = space.dimensions;
        for ((key, _), v) in batch.iter().zip(vectors) {
            if v.len() != want {
                anyhow::bail!(
                    "embedding for {} {key} has {} dimensions, expected {want}",
                    t.name,
                    v.len()
                );
            }
            written += sqlx::query(t.update)
                .bind(Vector::from(v))
                .bind(key)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        if written == 0 {
            // Nothing was updated although rows were selected: bail rather than spin.
            anyhow::bail!(
                "{}: selected {} unembedded rows but updated none",
                t.name,
                batch.len()
            );
        }
        total += usize::try_from(written)?;
        tracing::debug!(table = t.name, batch = batch.len(), total, "embedded batch");
    }
}

/// An embedding failure as the step reports it. A refusal by the spend cap
/// says so and how to get past it: the rows embedded so far are kept, so the
/// same command run again with room under the cap resumes where it stopped.
fn refused(e: &judge_core::JudgeError, done: usize) -> anyhow::Error {
    if judge_embed::is_spend_cap(e) {
        anyhow::anyhow!(
            "stopped at the spend cap after {done} rows of this table ({e}); the rows embedded so far are kept. \
             Raise JUDGE_MAX_USD for this run (`JUDGE_MAX_USD=20 judgebot ingest embed`, or with \
             JUDGE_BUDGET_PERIOD=day|month wait for the next period) and run it again to embed the rest"
        )
    } else {
        anyhow::anyhow!("embedder: {e}")
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

/// A fixed-vector embedder for the `embed` and `reembed` tests: no network,
/// counts its calls, can answer at a width other than its space's, and can
/// stage a concurrent `ingest reembed` by switching the database from inside
/// its first call (the one moment a switch can land between a batch's request
/// and its write). Behind a free meter, as every embedder is behind one.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use judge_core::InputKind;
    use judge_embed::{
        EmbedBackend, EmbedError, EmbedPrice, EmbedUsage, Embedded, MeteredEmbedder, Provider,
        Space,
    };
    use judge_llm::SpendMeter;
    use sqlx::PgPool;

    use crate::db::space::switch_space;

    pub(crate) struct Backend {
        space: Space,
        calls: AtomicUsize,
        /// Width of the vectors returned; the space's unless overridden.
        reply_dimensions: AtomicUsize,
        /// Performed on the first call, once.
        switch: Mutex<Option<(PgPool, Space)>>,
    }

    /// The fake, metered.
    pub(crate) type Fake = MeteredEmbedder<Backend>;

    /// A fake of `provider`/`model` at `dimensions`, on a free meter.
    pub(crate) fn fake_embedder(provider: Provider, model: &str, dimensions: usize) -> Fake {
        billed(
            provider,
            model,
            dimensions,
            &SpendMeter::new(),
            EmbedPrice::Free,
        )
    }

    /// [`fake_embedder`] billed to `meter` at `price`.
    pub(crate) fn billed(
        provider: Provider,
        model: &str,
        dimensions: usize,
        meter: &SpendMeter,
        price: EmbedPrice,
    ) -> Fake {
        MeteredEmbedder::priced(
            Backend {
                space: Space {
                    provider,
                    model: model.to_owned(),
                    dimensions,
                },
                calls: AtomicUsize::new(0),
                reply_dimensions: AtomicUsize::new(dimensions),
                switch: Mutex::new(None),
            },
            meter.clone(),
            price,
        )
    }

    pub(crate) trait FakeExt: Sized {
        /// Answer with vectors `n` wide regardless of the space.
        fn replying(self, n: usize) -> Self;
        /// Run `switch_space(pool, to)` inside the first `embed` call.
        fn switching_on_first_call(self, pool: PgPool, to: Space) -> Self;
        /// Requests that reached the backend.
        fn calls(&self) -> usize;
    }

    impl FakeExt for Fake {
        fn replying(self, n: usize) -> Self {
            self.inner().reply_dimensions.store(n, Ordering::SeqCst);
            self
        }

        fn switching_on_first_call(self, pool: PgPool, to: Space) -> Self {
            *self
                .inner()
                .switch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((pool, to));
            self
        }

        fn calls(&self) -> usize {
            self.inner().calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl EmbedBackend for Backend {
        async fn embed(&self, texts: &[&str], _kind: InputKind) -> Result<Embedded, EmbedError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let staged = self
                .switch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((pool, to)) = staged {
                switch_space(&pool, &to).await?;
            }
            let n = self.reply_dimensions.load(Ordering::SeqCst);
            Ok(Embedded {
                vectors: texts.iter().map(|_| vec![0.5; n]).collect(),
                usage: EmbedUsage::Tokens(u64::try_from(texts.len()).unwrap_or(0)),
            })
        }
        fn space(&self) -> &Space {
            &self.space
        }
    }
}

#[cfg(test)]
mod tests {
    use judge_embed::Provider;

    use super::{
        fake::{FakeExt as _, fake_embedder},
        *,
    };

    async fn glossary_row(pool: &PgPool) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO glossary (term, text, cr_version) VALUES ('Lifelink', 'A keyword ability.', '20260819')").execute(pool).await?;
        Ok(())
    }

    async fn embedded_glossary(pool: &PgPool) -> anyhow::Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM glossary WHERE embedding IS NOT NULL")
                .fetch_one(pool)
                .await?,
        )
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn first_embed_records_the_space_and_later_runs_only_fill_nulls(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        glossary_row(&pool).await?;
        assert_eq!(stored_space(&pool).await?, None);
        let fake = fake_embedder(Provider::Voyage, "voyage-3.5", 1024);
        run(&mut lease, Some(&fake)).await?;
        assert_eq!(stored_space(&pool).await?, Some(fake.space().clone()));
        assert_eq!(embedded_glossary(&pool).await?, 1);
        assert_eq!(fake.calls(), 1);
        run(&mut lease, Some(&fake)).await?;
        assert_eq!(fake.calls(), 1, "nothing left to embed");
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_batch_the_cap_cannot_fit_fails_the_step_unsent(pool: PgPool) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        glossary_row(&pool).await?;
        let meter = judge_llm::SpendMeter::new().with_max_spend_usd(0.0)?;
        let capped = super::fake::billed(
            Provider::Voyage,
            "voyage-3.5",
            1024,
            &meter,
            judge_embed::EmbedPrice::Table(0.06),
        );
        let err = run(&mut lease, Some(&capped))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("stopped at the spend cap") && err.contains("JUDGE_MAX_USD"),
            "{err}"
        );
        assert_eq!((capped.calls(), embedded_glossary(&pool).await?), (0, 0));
        // With room, the same embedder fills the row, billed to the meter.
        meter.set_max_spend_usd(1.0)?;
        run(&mut lease, Some(&capped)).await?;
        assert_eq!((capped.calls(), embedded_glossary(&pool).await?), (1, 1));
        assert_eq!(meter.embedding_calls(), 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_run_that_writes_nothing_leaves_no_label(pool: PgPool) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        // Nothing to embed: the row is written with the first vector, not before it.
        let fake = fake_embedder(Provider::Voyage, "voyage-3.5", 1024);
        run(&mut lease, Some(&fake)).await?;
        assert_eq!((stored_space(&pool).await?, fake.calls()), (None, 0));
        // A first batch that fails (the wrong width comes back) leaves no label either,
        // so the corrected run is a first embed, not a "mismatch" pointing at reembed.
        glossary_row(&pool).await?;
        let wrong = fake_embedder(Provider::OpenAi, "nomic-embed-text:v1.5", 1024).replying(768);
        let err = run(&mut lease, Some(&wrong))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("has 768 dimensions, expected 1024"), "{err}");
        assert_eq!(
            (stored_space(&pool).await?, embedded_glossary(&pool).await?),
            (None, 0)
        );
        let fixed = fake_embedder(Provider::OpenAi, "nomic-embed-text", 1024);
        run(&mut lease, Some(&fixed)).await?;
        assert_eq!(
            (stored_space(&pool).await?, embedded_glossary(&pool).await?),
            (Some(fixed.space().clone()), 1)
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_different_space_is_refused_before_anything_is_embedded(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        glossary_row(&pool).await?;
        let voyage = Space {
            provider: Provider::Voyage,
            model: "voyage-3.5".into(),
            dimensions: 1024,
        };
        record_space(&pool, &voyage).await?;
        let fake = fake_embedder(Provider::OpenAi, "nomic-embed-text", 1024);
        let err = run(&mut lease, Some(&fake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("embedding space mismatch")
                && err.contains("openai/nomic-embed-text (1024 dims)")
                && err.contains("voyage/voyage-3.5 (1024 dims)"),
            "{err}"
        );
        assert_eq!(fake.calls(), 0);
        assert_eq!(embedded_glossary(&pool).await?, 0);
        assert_eq!(stored_space(&pool).await?, Some(voyage));
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_switch_that_lands_mid_batch_is_seen_by_the_write(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        glossary_row(&pool).await?;
        let voyage = Space {
            provider: Provider::Voyage,
            model: "voyage-3.5".into(),
            dimensions: 1024,
        };
        record_space(&pool, &voyage).await?;
        // The check passes (the row is voyage), the request goes out, and while it is
        // in flight `ingest reembed --yes` moves the database to another 1024-wide
        // model. The batch's write must not land those Voyage vectors under the new row.
        let nomic = Space {
            provider: Provider::OpenAi,
            model: "nomic-embed-text".into(),
            dimensions: 1024,
        };
        let fake = fake_embedder(Provider::Voyage, "voyage-3.5", 1024)
            .switching_on_first_call(pool.clone(), nomic.clone());
        let err = run(&mut lease, Some(&fake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("changed while embedding") && err.contains("embedding space mismatch"),
            "{err}"
        );
        assert_eq!(fake.calls(), 1);
        assert_eq!(
            embedded_glossary(&pool).await?,
            0,
            "nothing of the old space was written"
        );
        assert_eq!(stored_space(&pool).await?, Some(nomic));
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn a_width_the_columns_do_not_have_points_at_reembed(pool: PgPool) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        glossary_row(&pool).await?;
        let fake = fake_embedder(Provider::OpenAi, "nomic-embed-text", 768);
        let err = run(&mut lease, Some(&fake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("rules.embedding is vector(1024)")
                && err.contains("768-dimensional")
                && err.contains("ingest reembed --yes"),
            "{err}"
        );
        assert!(
            !err.contains("the database holds"),
            "no row, nothing to name: {err}"
        );
        assert_eq!(fake.calls(), 0);
        assert_eq!(
            stored_space(&pool).await?,
            None,
            "the space is not recorded on a refusal"
        );
        // With a row, the message names the stored space so the operator can fix the
        // configuration instead of re-embedding.
        let voyage = Space {
            provider: Provider::Voyage,
            model: "voyage-3.5".into(),
            dimensions: 1024,
        };
        record_space(&pool, &voyage).await?;
        let err = run(&mut lease, Some(&fake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains(
                "the database holds voyage/voyage-3.5 (1024 dims): configure that model, or"
            ) && err.contains("ingest reembed --yes"),
            "{err}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../bot/migrations")]
    async fn vectors_of_unknown_origin_are_not_labelled(pool: PgPool) -> anyhow::Result<()> {
        let mut lease = crate::ingest::lease(&pool, "test").await?;
        glossary_row(&pool).await?;
        sqlx::query("UPDATE glossary SET embedding = $1")
            .bind(Vector::from(vec![0.1; 1024]))
            .execute(&pool)
            .await?;
        let fake = fake_embedder(Provider::Voyage, "voyage-3.5", 1024);
        let err = run(&mut lease, Some(&fake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("1 vectors are stored but embedding_space is empty"),
            "{err}"
        );
        assert_eq!(stored_space(&pool).await?, None);
        Ok(())
    }

    #[test]
    fn fit_chars_keeps_at_least_one_and_respects_cap() {
        let big = "x".repeat(MAX_BATCH_CHARS);
        let pairs = vec![
            ("a".to_owned(), big.clone()),
            ("b".to_owned(), big),
            ("c".to_owned(), "s".to_owned()),
        ];
        assert_eq!(fit_chars(&pairs).len(), 1);
        let small: Vec<(String, String)> =
            (0..10).map(|i| (i.to_string(), "t".to_owned())).collect();
        assert_eq!(fit_chars(&small).len(), 10);
        assert!(fit_chars(&[]).is_empty());
    }
}
