//! `api` — the HTTP binary: reads the environment, builds the shared
//! composition ([`judge_bot::build_deps`]) plus the call store, and serves
//! the axum routes and the built web client.
//!
//! Environment (a `.env` in the working directory is loaded first, like the
//! other binaries; the process environment wins): `DATABASE_URL`,
//! `ANTHROPIC_API_KEY`, optional `VOYAGE_API_KEY`, `API_ADDR`, `WEB_DIST`,
//! `JUDGE_CONCURRENCY`, `JUDGE_MAX_USD`, `API_RATE_LIMIT`,
//! `API_RATE_WINDOW_SECS`, `API_CLIENT_IP`, `RUST_LOG`; optional `MCP_TOKEN`
//! (mounts the MCP transport at `/mcp` behind it), `MCP_ALLOWED_HOSTS`,
//! `MCP_JUDGE_LIMIT` and `MCP_JUDGE_WINDOW_SECS`.
//! No Discord variables are read: the bot and the API are separate processes
//! sharing only the database.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use judge_agent::{Options, Quota, Toolbox};
use judge_api::{ApiConfig, App, router, serve};
use judge_bot::{Models, build_deps, db::PgCallStore, synth::Harness};
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
    // The zero-config models (Anthropic direct, one spend cap); the meter
    // handed to the HTTP layer is the one they bill to.
    let models = Models::from_env()?;
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
    let deps = build_deps(pool.clone(), &models, embedder.clone());
    let app = Arc::new(App::new(deps, store, models.meter().clone(), &cfg));
    let mut routes = router(Arc::clone(&app), &cfg.web_dist);
    // The MCP transport shares the judge slots (one JUDGE_CONCURRENCY for
    // both front doors) and the metered models (one cap).
    if let Some(token) = &cfg.mcp_token {
        let toolbox = Toolbox::new(
            pool,
            Options {
                harness: Harness::Mcp,
                models: Some(models),
                embedder,
                permits: app.permits(),
                judge_quota: Some(Quota { limit: cfg.mcp_judge_limit, window: cfg.mcp_judge_window }),
                history_len: cfg.history_len,
            },
        );
        let service = judge_agent::mcp::http_service(Arc::new(toolbox), cfg.mcp_hosts.clone());
        routes = routes.merge(judge_api::mcp::router(service, token));
    }
    tracing::info!(
        rate_limit = cfg.rate_limit,
        rate_window_secs = cfg.rate_window.as_secs(),
        concurrency = cfg.max_concurrent,
        mcp = cfg.mcp_token.is_some(),
        "starting HTTP adapter"
    );
    serve(&cfg, routes).await
}
