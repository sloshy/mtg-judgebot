//! `bot` — the Discord binary: reads the environment, builds the shared
//! composition ([`judge_bot::build_deps`]) plus the call store, and runs
//! serenity/poise ([`judge_bot::discord::run`]). Build-order step 6.
//!
//! Environment (a `.env` in the working directory is loaded first, like the
//! `eval` and `ingest` binaries; the process environment wins): `DISCORD_TOKEN`
//! (required), `DATABASE_URL`, `ANTHROPIC_API_KEY`, optional `VOYAGE_API_KEY`,
//! `GUILD_ID`, `JUDGE_ROLE`, `JUDGE_CONCURRENCY`, `JUDGE_MAX_USD`, `RUST_LOG`;
//! optional `JUDGE_CONFIG` (or a `./judge.toml`) picks other providers and
//! models ([`judge_bot::config`]). A missing token is reported before
//! anything else is touched and exits non-zero.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use judge_bot::{
    build_deps_with,
    config::Config as JudgeConfig,
    db::{PgCallStore, PgLibrary},
    discord::{Config, Data, run},
};
use judge_core::CallStore;

#[tokio::main]
async fn main() -> Result<()> {
    // A missing .env is fine; a malformed one is not.
    match dotenvy::dotenv() {
        Ok(_) | Err(dotenvy::Error::Io(_)) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("load .env")),
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // Fail fast on the one thing nothing else can substitute for.
    let cfg = Config::from_env()?;
    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is not set"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("connect to Postgres")?;
    // The schema first: pending migrations are applied here (opt out with
    // JUDGE_AUTO_MIGRATE=false), and a failure exits so the restart policy
    // makes it loud rather than answering without persisting.
    judge_bot::db::migrate::at_startup(&pool)
        .await
        .context("migrating the schema")?;
    // The models (judge.toml, or the zero-config Anthropic setup; one spend
    // cap); the meter handed to the Discord layer is the one they bill to.
    let judge = JudgeConfig::load()?;
    tracing::info!("{}", judge.summary());
    // The bot names who runs it: no JUDGE_OPERATOR_DISCORD, no bot.
    let operator = judge.discord_operator()?;
    tracing::info!(discord = %operator.username(), "operator contact");
    let models = judge.models()?;
    // A cloud door with no credentials fails here, not on the first question.
    judge.probe_auth().await?;
    // The embedder is optional: without one the retriever skips its vector
    // leg. One `Vectors` for the store and the retriever: the space check
    // against `embedding_space` runs once and disables both on a mismatch.
    let vectors = judge.vectors(pool.clone())?;
    // The verdict (on, absent row, mismatch) lands here beside the summary, not in the first request's log.
    if let Some(v) = &vectors {
        v.enabled().await;
    } else {
        tracing::warn!(
            "no embedder configured (VOYAGE_API_KEY or [models.embed]); running without the vector leg"
        );
    }
    let mut store = PgCallStore::new(pool.clone());
    if let Some(v) = &vectors {
        store = store.with_vectors(Arc::clone(v));
    }
    let store: Arc<dyn CallStore> = Arc::new(store);
    let meter = models.meter().clone();
    // `/card` reads rulings by card id only, so its library needs no vectors.
    let library = PgLibrary::new(pool.clone());
    let deps = build_deps_with(pool, &models, vectors, &judge.deps_config());
    tracing::info!(source = %judge.source_offer(), "source offer");
    let data = Data::new(
        deps,
        store,
        meter,
        &cfg,
        judge.source_offer().clone(),
        operator,
        library,
    );
    tracing::info!(
        guild = ?cfg.guild_id,
        judge_role = %cfg.judge_role,
        concurrency = cfg.max_concurrent,
        user_limit = ?cfg.user_limit,
        "starting Discord adapter"
    );
    run(cfg, data).await
}
