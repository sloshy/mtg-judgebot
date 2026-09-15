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
//! `MCP_JUDGE_LIMIT` and `MCP_JUDGE_WINDOW_SECS`; optional `JUDGE_CONFIG` (or
//! a `./judge.toml`) picks other providers and models ([`judge_bot::config`]).
//! No Discord variables are read: the bot and the API are separate processes
//! sharing only the database.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use judge_agent::{Options, Quota, Toolbox};
use judge_api::{ApiConfig, App, router, serve};
use judge_bot::{build_deps_with, config::Config as JudgeConfig, db::PgCallStore, synth::Harness};
use judge_core::CallStore;
use judge_llm::ApiKey;

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
    // The schema first: pending migrations are applied here (opt out with
    // JUDGE_AUTO_MIGRATE=false), and a failure exits so the restart policy
    // makes it loud rather than answering without persisting.
    judge_bot::db::migrate::at_startup(&pool)
        .await
        .context("migrating the schema")?;
    // The models (judge.toml, or the zero-config Anthropic setup; one spend
    // cap); the meter handed to the HTTP layer is the one they bill to.
    let judge = JudgeConfig::load()?;
    tracing::info!("{}", judge.summary());
    let models = judge.models()?;
    // A cloud door with no credentials fails here, not on the first question.
    judge.probe_auth().await?;
    // The embedder is optional: without one the retriever skips its vector
    // leg. One `Vectors` for every adapter in the process: the space check
    // against `embedding_space` runs once and disables them all on a mismatch.
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
    let deps = build_deps_with(pool.clone(), &models, vectors.clone(), &judge.deps_config());
    let app = Arc::new(App::new(deps, store, models.meter().clone(), &cfg));
    let mut routes = router(Arc::clone(&app), &cfg.web_dist);
    // The MCP transport shares the judge slots (one JUDGE_CONCURRENCY for
    // both front doors) and the metered models (one cap).
    if let Some(token) = cfg.mcp_token.as_ref().map(ApiKey::expose) {
        let toolbox = Toolbox::new(
            pool,
            Options {
                harness: Harness::Mcp,
                models: Some(models),
                deps_config: judge.deps_config(),
                vectors,
                permits: app.permits(),
                judge_quota: Some(Quota {
                    limit: cfg.mcp_judge_limit,
                    window: cfg.mcp_judge_window,
                }),
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
