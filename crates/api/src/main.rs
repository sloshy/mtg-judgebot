//! `api` — the HTTP binary: reads the command line and the environment, builds
//! the shared composition ([`judge_bot::build_deps`]) plus the call store, and
//! serves the front doors it was asked for.
//!
//! Command line (`judge_api::interfaces`): every front door is opt-in, and
//! `judge-api` with no arguments serves the JSON API alone. `--web` adds (or,
//! alone, substitutes) the built web client, `--mcp` the MCP transport;
//! `--help` prints the usage. `GET /api/health` is served either way.
//!
//! Environment (a `.env` in the working directory is loaded first, like the
//! other binaries; the process environment wins): `DATABASE_URL`,
//! `ANTHROPIC_API_KEY`, optional `VOYAGE_API_KEY`, `API_ADDR`,
//! `JUDGE_CONCURRENCY`, `JUDGE_MAX_USD`, `API_RATE_LIMIT`,
//! `API_RATE_WINDOW_SECS`, `API_CLIENT_IP`, `RUST_LOG`; `WEB_DIST` under
//! `--web`; `MCP_TOKEN` (required by `--mcp`),
//! `MCP_ALLOWED_HOSTS`, `MCP_JUDGE_LIMIT` and `MCP_JUDGE_WINDOW_SECS` (a token
//! without `--mcp` is a startup warning, not an endpoint);
//! optional `JUDGE_CONFIG` (or a `./judge.toml`) picks other providers and
//! models ([`judge_bot::config`]). No Discord variables are read: the bot and
//! the API are separate processes sharing only the database.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use judge_agent::{Options, Quota, Toolbox};
use judge_api::{ApiConfig, App, Launch, interfaces, router, serve};
use judge_bot::{build_deps_with, config::Config as JudgeConfig, db::PgCallStore, synth::Harness};
use judge_core::CallStore;
use judge_llm::ApiKey;

#[tokio::main]
async fn main() -> Result<()> {
    // The command line before anything else: `--help` and a typo'd flag must
    // not need a database, a key or a working directory to answer.
    let interfaces = match interfaces::parse(std::env::args_os().skip(1))? {
        Launch::Help => {
            println!("{}", interfaces::USAGE);
            return Ok(());
        }
        Launch::Serve(i) => i,
    };
    // A missing .env is fine; a malformed one is not.
    match dotenvy::dotenv() {
        Ok(_) | Err(dotenvy::Error::Io(_)) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("load .env")),
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cfg = ApiConfig::from_env()?;
    // An interface the environment cannot satisfy fails here, not as a 404 or
    // an open endpoint later.
    cfg.check(&interfaces)?;
    // The mismatches that cost nothing but a misconception are warnings, not
    // refusals: an api container that will not start takes the *page* down
    // over an MCP mistake, and `MCP_TOKEN` is still a valid variable an
    // operator may simply have left in place.
    if cfg.mcp_token.is_some() && !interfaces.mcp() {
        tracing::warn!(
            "MCP_TOKEN is set but --mcp was not given, so /mcp is not served; \
             add --mcp (API_INTERFACES in .env, for the compose api service) \
             or unset the token"
        );
    }
    if interfaces.web() && !interfaces.api() {
        tracing::warn!(
            "--web without --api: the page is served but POST /api/judge is not, \
             so questions asked on it fail (405 from the static file service) \
             unless another process answers them"
        );
    }
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
    // Every door names who runs it: no JUDGE_OPERATOR_EMAIL, no server.
    let operator = judge.network_operator()?;
    tracing::info!(email = %operator.email(), "operator contact");
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
    // /api/health answers 503 when Postgres does not: the compose healthcheck
    // and the tunnel's readiness key off it.
    tracing::info!(source = %judge.source_offer(), "source offer");
    let app = Arc::new(
        App::new(
            deps,
            store,
            models.meter().clone(),
            &cfg,
            judge.source_offer().clone(),
            operator.clone(),
        )
        .with_probe(Arc::new(pool.clone())),
    );
    let mut routes = router(Arc::clone(&app), &interfaces, &cfg.web_dist);
    // The MCP transport shares the judge slots (one JUDGE_CONCURRENCY for
    // both front doors) and the metered models (one cap). It takes the flag
    // *and* the token: `check` has already refused `--mcp` without one, and
    // the filter is what makes a token alone inert rather than a mount.
    if let Some(token) = cfg
        .mcp_token
        .as_ref()
        .filter(|_| interfaces.mcp())
        .map(ApiKey::expose)
    {
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
                offer: judge.source_offer().clone(),
                operator: operator.operator().clone(),
            },
        );
        let service = judge_agent::mcp::http_service(Arc::new(toolbox), cfg.mcp_hosts.clone());
        routes = routes.merge(judge_api::mcp::router(service, token));
    }
    // The enabled *and* the disabled doors: an operator hunting a 404 reads
    // the reason here rather than inferring it from the routes that answer.
    tracing::info!(
        interfaces = %interfaces,
        off = %interfaces.disabled(),
        web_dist = interfaces.web().then(|| cfg.web_dist.display().to_string()),
        rate_limit = cfg.rate_limit,
        rate_window_secs = cfg.rate_window.as_secs(),
        concurrency = cfg.max_concurrent,
        "starting HTTP adapter"
    );
    serve(&cfg, routes).await
}
