//! `bot` — the Discord binary: reads the environment, builds the shared
//! composition ([`judge_bot::build_deps`]) plus the call store, and runs
//! serenity/poise. Build-order step 6.

use std::sync::Arc;

use anyhow::Result;
use judge_bot::{build_deps, db::PgCallStore};
use judge_core::{CallStore, Embedder};
use judge_embed::VoyageEmbedder;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let database_url = std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is not set"))?;
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&database_url).await?;
    // One HTTP client (connection pool) shared by every Anthropic adapter.
    let anthropic = judge_anthropic::Client::from_env()?;
    // The embedder is optional: without a Voyage key the retriever skips its vector leg.
    let embedder: Option<Arc<dyn Embedder>> = if std::env::var_os("VOYAGE_API_KEY").is_some() {
        Some(Arc::new(VoyageEmbedder::from_env()?))
    } else {
        tracing::warn!("VOYAGE_API_KEY is not set; running without the vector leg");
        None
    };
    // The store is handed to the Discord adapter (rating buttons) once it exists.
    let mut store = PgCallStore::new(pool.clone());
    if let Some(e) = &embedder {
        store = store.with_embedder(Arc::clone(e));
    }
    let _store: Arc<dyn CallStore> = Arc::new(store);
    let _deps = build_deps(pool, anthropic, embedder);
    todo!("start serenity/poise")
}
