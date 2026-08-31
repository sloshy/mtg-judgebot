//! `api` — the HTTP binary: reads the environment, builds the shared
//! composition ([`judge_bot::build_deps`]) plus the call store, and serves
//! the axum routes and the built web client.
//!
//! Environment (a `.env` in the working directory is loaded first, like the
//! other binaries; the process environment wins): `DATABASE_URL`,
//! `ANTHROPIC_API_KEY`, optional `VOYAGE_API_KEY`, `API_ADDR`, `WEB_DIST`,
//! `JUDGE_CONCURRENCY`, `JUDGE_MAX_USD`, `API_RATE_LIMIT`,
//! `API_RATE_WINDOW_SECS`, `API_TRUST_FORWARDED`, `RUST_LOG`. No Discord
//! variables are read: the bot and the API are separate processes sharing
//! only the database.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use judge_api::{ApiConfig, App, serve};
use judge_bot::{build_deps, db::PgCallStore};
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
    let cfg = ApiConfig::from_env()?;
    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is not set"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("connect to Postgres")?;
    // One HTTP client (connection pool) shared by every Anthropic adapter; the
    // clone handed to the HTTP layer shares its spend counters.
    let anthropic = judge_anthropic::Client::from_env()?;
    // The embedder is optional: without a Voyage key the retriever skips its vector leg.
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
    let app = Arc::new(App::new(deps, store, anthropic, &cfg));
    tracing::info!(
        rate_limit = cfg.rate_limit,
        rate_window_secs = cfg.rate_window.as_secs(),
        concurrency = cfg.max_concurrent,
        "starting HTTP adapter"
    );
    serve(&cfg, app).await
}
