//! `bot` — the Discord binary: reads the environment, builds the shared
//! composition ([`judge_bot::build_deps`]) plus the call store, and runs
//! serenity/poise ([`judge_bot::discord::run`]). Build-order step 6.
//!
//! Environment (a `.env` in the working directory is loaded first, like the
//! `eval` and `ingest` binaries; the process environment wins): `DISCORD_TOKEN`
//! (required), `DATABASE_URL`, `ANTHROPIC_API_KEY`, optional `VOYAGE_API_KEY`,
//! `GUILD_ID`, `JUDGE_ROLE`, `JUDGE_CONCURRENCY`, `JUDGE_MAX_USD`, `RUST_LOG`.
//! A missing token is reported before anything else is touched and exits
//! non-zero.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use judge_bot::{
    build_deps,
    db::PgCallStore,
    discord::{Config, Data, run},
};
use judge_core::{CallStore, Embedder};
use judge_embed::VoyageEmbedder;

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
    // One HTTP client (connection pool) shared by every Anthropic adapter; the
    // clone handed to the Discord layer shares its spend counters.
    let anthropic = judge_anthropic::Client::from_env()?;
    // The embedder is optional: without a Voyage key the retriever skips its vector leg.
    // A blank value (`VOYAGE_API_KEY=` in .env) counts as unset, matching `VoyageEmbedder::from_env`.
    let has_voyage_key = std::env::var("VOYAGE_API_KEY").is_ok_and(|k| !k.trim().is_empty());
    let embedder: Option<Arc<dyn Embedder>> = if has_voyage_key {
        Some(Arc::new(VoyageEmbedder::from_env()?))
    } else {
        tracing::warn!("VOYAGE_API_KEY is not set; running without the vector leg");
        None
    };
    let mut store = PgCallStore::new(pool.clone());
    if let Some(e) = &embedder {
        store = store.with_embedder(Arc::clone(e));
    }
    let store: Arc<dyn CallStore> = Arc::new(store);
    let deps = build_deps(pool, anthropic.clone(), embedder);
    let data = Data::new(deps, store, anthropic, &cfg);
    tracing::info!(
        guild = ?cfg.guild_id,
        judge_role = %cfg.judge_role,
        concurrency = cfg.max_concurrent,
        "starting Discord adapter"
    );
    run(cfg, data).await
}
