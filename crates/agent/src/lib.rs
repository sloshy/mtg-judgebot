//! `judge-agent` — the judge as a tool surface for *other* agents.
//!
//! Two ways in, one set of operations ([`ops`]):
//!
//! * **MCP** ([`mcp`]): `judge-mcp` speaks the protocol over stdio for a
//!   local client, and `judge-api` mounts the same handler over Streamable
//!   HTTP for a remote one.
//! * **CLI** (`judge-cli`): every operation is a subcommand printing JSON, for
//!   a shell agent (a Claude Code skill) or a person.
//!
//! And two ways to get an answer:
//!
//! * `judge`: the whole pipeline with the built-in model calls — the same
//!   `judge()` the Discord bot and the web page run, spending the operator's
//!   API budget. Only offered when a model is configured (`ANTHROPIC_API_KEY`,
//!   or a `judge.toml`).
//! * A **session** (`judge_bot::session`): the pipeline in pull mode. The
//!   caller receives the extraction prompt, answers it, receives the
//!   synthesis prompt, may look rules up once, answers it, and the verdict is
//!   validated with the same citation check. No API call is made; the
//!   calling agent *is* the model.
//!
//! [`Toolbox`] holds the ports both need. Its spend cap, judge slots and
//! `judge` quota are per process: inside `judge-api` they are the ones the
//! web page uses, so the two front doors share one budget; a `judge-cli`
//! invocation or a stdio `judge-mcp` is its own process with its own
//! `JUDGE_MAX_USD` counter, which is fine for the operator's own shell and
//! is why the remote transport is `/mcp` and not a stdio server exposed to
//! others.

pub mod mcp;
pub mod ops;
#[cfg(test)]
mod tests;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use judge_bot::{
    DepsConfig, Models,
    config::Config,
    db::{PgCallStore, PgLibrary, PgResolver, PgRetriever, PgSessionStore, Vectors},
    discord::capture::CapturingRetriever,
    session::{PersistCall, Sessions},
    synth::Harness,
};
use judge_core::{CallStore, Deps, Resolver, Retriever};
use judge_llm::SpendMeter;
use sqlx::PgPool;
use tokio::sync::Semaphore;

/// How long `judge` waits for a free pipeline slot before answering "busy".
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(10);
/// Thread history handed to the pipeline and to sessions.
pub const DEFAULT_HISTORY: usize = 5;
/// `JUDGE_CONCURRENCY` default, as for the other front doors.
pub const DEFAULT_CONCURRENCY: usize = 2;

/// The built-in pipeline, present when models are configured.
pub struct Pipeline {
    deps: Deps,
    capture: Arc<CapturingRetriever>,
    /// The meter the pipeline's models bill to.
    meter: SpendMeter,
}

/// How many `judge` runs one toolbox allows per window: the blast radius of
/// a leaked remote token, in pipeline runs rather than only in dollars. A
/// leaked token would otherwise take every judge slot and the whole spend
/// cap away from the anonymous web page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    /// Runs per window.
    pub limit: u32,
    /// The fixed window.
    pub window: Duration,
}

/// Fixed-window counter for [`Quota`].
#[derive(Debug)]
struct Meter {
    quota: Quota,
    started: Instant,
    count: u32,
}

impl Meter {
    fn allow(&mut self, now: Instant) -> bool {
        if now.duration_since(self.started) >= self.quota.window {
            self.started = now;
            self.count = 0;
        }
        if self.count >= self.quota.limit {
            return false;
        }
        self.count = self.count.saturating_add(1);
        true
    }
}

/// Everything the operations need.
pub struct Toolbox {
    sessions: Sessions,
    library: PgLibrary,
    resolver: Arc<dyn Resolver>,
    retriever: Arc<dyn Retriever>,
    calls: Arc<dyn CallStore>,
    pipeline: Option<Pipeline>,
    permits: Arc<Semaphore>,
    meter: Option<Mutex<Meter>>,
    history_len: usize,
}

impl std::fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolbox")
            .field("pipeline", &self.pipeline.is_some())
            .field("permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

/// How to build a [`Toolbox`].
pub struct Options {
    /// Which harness the session prompts are worded for.
    pub harness: Harness,
    /// The metered models for the built-in pipeline; `None` disables `judge`.
    pub models: Option<Models>,
    /// The request knobs the models were configured with.
    pub deps_config: DepsConfig,
    /// The embedder behind its space check; `None` turns the vector legs off.
    pub vectors: Option<Arc<Vectors>>,
    /// Pipeline slots, shared with any other front door in the same process.
    pub permits: Arc<Semaphore>,
    /// A cap on `judge` runs per window for this toolbox; `None` for a local
    /// operator's own process.
    pub judge_quota: Option<Quota>,
    /// Thread history length.
    pub history_len: usize,
}

impl Toolbox {
    /// Wire the ports over `pool`.
    #[must_use]
    pub fn new(pool: PgPool, opts: Options) -> Self {
        let mut retriever = PgRetriever::new(pool.clone());
        let mut library = PgLibrary::new(pool.clone());
        let mut calls = PgCallStore::new(pool.clone());
        if let Some(v) = &opts.vectors {
            retriever = retriever.with_vectors(Arc::clone(v));
            library = library.with_vectors(Arc::clone(v));
            calls = calls.with_vectors(Arc::clone(v));
        }
        let resolver: Arc<dyn Resolver> = Arc::new(PgResolver::new(pool.clone()));
        let retriever: Arc<dyn Retriever> = Arc::new(retriever);
        let calls = Arc::new(calls);
        let sessions = Sessions::new(
            PgSessionStore::new(pool.clone()),
            Arc::clone(&resolver),
            Arc::clone(&retriever),
            Arc::clone(&calls) as Arc<dyn CallStore>,
            Arc::clone(&calls) as Arc<dyn PersistCall>,
            opts.harness,
        )
        .with_history_len(opts.history_len);
        let calls: Arc<dyn CallStore> = calls;
        let pipeline = opts.models.map(|models| {
            let meter = models.meter().clone();
            let mut deps = judge_bot::build_deps_with(pool, &models, opts.vectors, &opts.deps_config);
            let capture = Arc::new(CapturingRetriever::new(Arc::clone(&deps.retriever)));
            deps.retriever = Arc::clone(&capture) as Arc<dyn Retriever>;
            Pipeline { deps, capture, meter }
        });
        Self {
            sessions,
            library,
            resolver,
            retriever,
            calls,
            pipeline,
            permits: opts.permits,
            meter: opts.judge_quota.map(|quota| Mutex::new(Meter { quota, started: Instant::now(), count: 0 })),
            history_len: opts.history_len,
        }
    }

    /// Count one `judge` run against the quota; `false` means the window is
    /// used up. Always `true` without a quota.
    pub(crate) fn allow_judge(&self) -> bool {
        self.meter
            .as_ref()
            .is_none_or(|m| m.lock().unwrap_or_else(std::sync::PoisonError::into_inner).allow(Instant::now()))
    }

    /// Build from the environment: `DATABASE_URL` (required); the models
    /// from `JUDGE_CONFIG` / `./judge.toml`, else `ANTHROPIC_API_KEY`
    /// (optional: without either `judge` is unavailable and sessions still
    /// work) and `VOYAGE_API_KEY` (optional); `JUDGE_CONCURRENCY`,
    /// `JUDGE_MAX_USD` as for the other binaries. A `.env` in the working
    /// directory is loaded first.
    ///
    /// # Errors
    /// A missing `DATABASE_URL`, a malformed `.env`, a bad model
    /// configuration, or a failed connection.
    pub async fn from_env(harness: Harness) -> anyhow::Result<Self> {
        match dotenvy::dotenv() {
            Ok(_) | Err(dotenvy::Error::Io(_)) => {}
            Err(e) => return Err(anyhow::Error::from(e).context("load .env")),
        }
        let set = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let database_url = set("DATABASE_URL").context("DATABASE_URL is not set")?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .context("connect to Postgres")?;
        let config = Config::load()?;
        tracing::info!("{}", config.summary());
        let models = config.models_if_configured().context("models")?;
        if models.is_none() {
            tracing::info!("no model configured (ANTHROPIC_API_KEY or a judge.toml); the built-in `judge` pipeline is unavailable, sessions are not affected");
        }
        let vectors = config.vectors(pool.clone())?;
        // Logged at startup so a mismatch is visible before the first lookup.
        if let Some(v) = &vectors {
            v.enabled().await;
        } else {
            tracing::info!("no embedder configured; running without the vector legs");
        }
        let concurrency = match set("JUDGE_CONCURRENCY") {
            Some(v) => v.trim().parse::<usize>().context("JUDGE_CONCURRENCY must be an integer")?.max(1),
            None => DEFAULT_CONCURRENCY,
        };
        Ok(Self::new(
            pool,
            Options {
                harness,
                models,
                deps_config: config.deps_config(),
                vectors,
                permits: Arc::new(Semaphore::new(concurrency)),
                judge_quota: None,
                history_len: DEFAULT_HISTORY,
            },
        ))
    }

    /// Whether the built-in pipeline can run.
    #[must_use]
    pub fn has_pipeline(&self) -> bool {
        self.pipeline.is_some()
    }
}

#[cfg(test)]
mod meter_tests {
    use super::*;

    #[test]
    fn the_window_resets_and_the_limit_holds() {
        let start = Instant::now();
        let mut m = Meter { quota: Quota { limit: 2, window: Duration::from_mins(1) }, started: start, count: 0 };
        assert!(m.allow(start));
        assert!(m.allow(start + Duration::from_secs(1)));
        assert!(!m.allow(start + Duration::from_secs(2)), "third run in the window");
        assert!(m.allow(start + Duration::from_mins(1)), "a new window");
        assert!(m.allow(start + Duration::from_secs(61)));
        assert!(!m.allow(start + Duration::from_secs(62)));
    }
}
