//! `judge-agent` — the judge as a tool surface for *other* agents.
//!
//! Two ways in, one set of operations ([`ops`]):
//!
//! * **MCP** ([`mcp`]): `judge-mcp` speaks the protocol over stdio for a
//!   local client, and `judgebot --mcp` mounts the same handler over Streamable
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
//! The operator's reads (every call, private ones included, the failed calls,
//! the usage) are operations too, each taking an [`admin::Admin`] capability
//! that only [`Toolbox::from_env`] (a standalone toolbox) or the
//! `MCP_ADMIN_TOKEN` handler hands out.
//!
//! [`Toolbox`] holds the ports both need. Its spend cap, judge slots and
//! `judge` quota are per process: inside `judgebot --mcp` they are the ones the
//! web page uses, so the two interfaces share one budget; a `judge-cli`
//! invocation or a stdio `judge-mcp` is its own process with its own
//! `JUDGE_MAX_USD` counter, which is fine for the operator's own shell and
//! is why the remote transport is `/mcp` and not a stdio server exposed to
//! others.

pub mod admin;
mod gated;
pub mod mcp;
pub mod ops;
pub mod release;
#[cfg(test)]
mod tests;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use judge_bot::maintenance::Availability;
use judge_bot::{
    DepsConfig, Models,
    config::Config,
    db::{PgCallStore, PgLibrary, PgResolver, PgRetriever, PgSessionStore, Vectors},
    session::{PersistCall, Sessions},
    synth::Harness,
};
use judge_core::{CallStore, Deps, Operator, Origin, Resolver, Retriever, SourceOffer, Visibility};
use sqlx::PgPool;
use tokio::sync::Semaphore;

/// How long `judge` waits for a free pipeline slot before answering "busy".
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(10);
/// Thread history handed to the pipeline and to sessions.
pub const DEFAULT_HISTORY: usize = 5;
/// `JUDGE_CONCURRENCY` default, as for the other interfaces.
pub const DEFAULT_CONCURRENCY: usize = 2;

/// The interface a toolbox serves. It decides what the toolbox's `judge`
/// calls are stored as and how the session prompts are worded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `judge-cli`.
    Cli,
    /// `judge-mcp` on stdio, or `/mcp` under `judgebot --mcp`.
    Mcp,
}

impl Transport {
    /// The session prompts' wording for this interface.
    #[must_use]
    pub const fn harness(self) -> Harness {
        match self {
            Self::Cli => Harness::Cli,
            Self::Mcp => Harness::Mcp,
        }
    }

    /// What a `judge` call made through this interface is stored as.
    #[must_use]
    pub const fn origin(self, visibility: Visibility) -> Origin {
        match self {
            Self::Cli => Origin::Cli(visibility),
            Self::Mcp => Origin::Mcp(visibility),
        }
    }

    /// The tool's name, as its messages call it.
    #[must_use]
    pub const fn tool(self) -> &'static str {
        match self {
            Self::Cli => "judge-cli",
            Self::Mcp => "judge-mcp",
        }
    }
}

/// Where a toolbox runs relative to the instance. Outside this crate only
/// [`Client::instance`] makes one: a standalone toolbox comes from
/// [`Toolbox::from_env`] alone, which is what hands out the admin
/// capability ([`Local`]).
#[derive(Clone, Debug)]
pub struct Client(ClientKind);

#[derive(Clone, Debug)]
enum ClientKind {
    /// A tool built on its own (`judge-cli`, a stdio `judge-mcp`): its
    /// release is checked against the database's ([`release`]).
    Standalone,
    /// Part of the instance (`judgebot --mcp`), its release by construction:
    /// it answers while the instance is open, and with the maintenance
    /// message while a major upgrade migrates.
    Instance(Availability),
}

impl Client {
    /// Part of the instance (`judgebot --mcp`), open or migrating as
    /// `availability` says.
    #[must_use]
    pub const fn instance(availability: Availability) -> Self {
        Self(ClientKind::Instance(availability))
    }

    /// A tool of its own, its release checked.
    pub(crate) const fn standalone() -> Self {
        Self(ClientKind::Standalone)
    }
}

/// A standalone toolbox ([`Toolbox::from_env`]) and the admin capability
/// that comes with it: whoever runs it holds `DATABASE_URL`, which reads
/// everything the admin operations do.
#[derive(Debug)]
pub struct Local {
    /// The toolbox.
    pub toolbox: Toolbox,
    /// The operator's reads.
    pub admin: admin::Admin,
}

/// The built-in pipeline, present when models are configured.
pub struct Pipeline {
    deps: Deps,
}

/// How many `judge` runs one toolbox allows per window: the most that can be spent through
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
    /// The ports over the database, behind the release check.
    db: gated::Db,
    transport: Transport,
    permits: Arc<Semaphore>,
    meter: Option<Mutex<Meter>>,
    history_len: usize,
    /// What `about` and the MCP instructions say about where this
    /// instance's source is.
    offer: SourceOffer,
    /// Who runs this instance, as far as they said.
    operator: Operator,
}

impl std::fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Toolbox")
            .field("pipeline", &self.db.has_pipeline())
            .field("permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

/// How to build a [`Toolbox`].
pub struct Options {
    /// The interface served: what `judge` calls are stored as, and how the
    /// session prompts are worded.
    pub transport: Transport,
    /// Whether this is a separate tool, whose release is checked. Only
    /// [`Client::instance`] outside this crate.
    pub client: Client,
    /// The metered models for the built-in pipeline; `None` disables `judge`.
    pub models: Option<Models>,
    /// The request knobs the models were configured with.
    pub deps_config: DepsConfig,
    /// The embedder behind its space check; `None` turns the vector search off.
    pub vectors: Option<Arc<Vectors>>,
    /// Pipeline slots, shared with any other interface in the same process.
    pub permits: Arc<Semaphore>,
    /// A cap on `judge` runs per window for this toolbox; `None` for a local
    /// operator's own process.
    pub judge_quota: Option<Quota>,
    /// Thread history length.
    pub history_len: usize,
    /// The source offer (`Config::source_offer`).
    pub offer: SourceOffer,
    /// Who runs this instance (`Config::operator`). Optional here because a
    /// local process has no one else to name; `judgebot --mcp` hands `/mcp` the
    /// one it refused to start without (`NetworkOperator::operator`).
    pub operator: Operator,
}

impl Toolbox {
    /// Wire the ports over `pool`.
    #[must_use]
    pub fn new(pool: PgPool, opts: Options) -> Self {
        let pool_for_release = pool.clone();
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
            opts.transport.harness(),
        )
        .with_history_len(opts.history_len);
        let calls: Arc<dyn CallStore> = calls;
        let pipeline = opts.models.map(|models| Pipeline {
            deps: judge_bot::build_deps_with(pool, &models, opts.vectors, &opts.deps_config),
        });
        Self {
            db: gated::Db::new(gated::Parts {
                sessions,
                library,
                resolver,
                retriever,
                calls,
                pipeline,
                guard: match opts.client.0 {
                    ClientKind::Standalone => gated::Guard::Release(release::ReleaseCheck::new(
                        pool_for_release,
                        opts.transport.tool(),
                    )),
                    ClientKind::Instance(availability) => gated::Guard::Instance(availability),
                },
            }),
            transport: opts.transport,
            permits: opts.permits,
            meter: opts.judge_quota.map(|quota| {
                Mutex::new(Meter {
                    quota,
                    started: Instant::now(),
                    count: 0,
                })
            }),
            history_len: opts.history_len,
            offer: opts.offer,
            operator: opts.operator,
        }
    }

    /// Who runs this instance.
    #[must_use]
    pub fn operator(&self) -> &Operator {
        &self.operator
    }

    /// The source offer this process makes.
    #[must_use]
    pub fn offer(&self) -> &SourceOffer {
        &self.offer
    }

    /// Count one `judge` run against the quota; `false` means the window is
    /// used up. Always `true` without a quota.
    pub(crate) fn allow_judge(&self) -> bool {
        self.meter.as_ref().is_none_or(|m| {
            m.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .allow(Instant::now())
        })
    }

    /// Build from the environment: `DATABASE_URL` (required); the models
    /// from `JUDGE_CONFIG` / `./judge.toml`, else `ANTHROPIC_API_KEY`
    /// (optional: without either `judge` is unavailable and sessions still
    /// work) and `VOYAGE_API_KEY` (optional); `JUDGE_CONCURRENCY`,
    /// `JUDGE_MAX_USD` as for the other binaries. A `.env` in the working
    /// directory is loaded first.
    ///
    /// The toolbox is standalone, so it comes with the admin capability
    /// ([`Local`]): the one way to hold one outside the `MCP_ADMIN_TOKEN`
    /// handler.
    ///
    /// # Errors
    /// A missing `DATABASE_URL`, a malformed `.env`, a bad model
    /// configuration, or a failed connection.
    pub async fn from_env(transport: Transport) -> anyhow::Result<Local> {
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
            tracing::info!(
                "no model configured (ANTHROPIC_API_KEY or a judge.toml); the built-in `judge` pipeline is unavailable, sessions are not affected"
            );
        } else {
            // A cloud endpoint with no credentials fails here, not on the first `judge` call.
            config.probe_auth().await?;
        }
        let vectors = config.vectors(pool.clone())?;
        // Logged at startup so a mismatch is visible before the first lookup.
        if let Some(v) = &vectors {
            v.enabled().await;
        } else {
            tracing::info!("no embedder configured; running without vector search");
        }
        let concurrency = match set("JUDGE_CONCURRENCY") {
            Some(v) => v
                .trim()
                .parse::<usize>()
                .context("JUDGE_CONCURRENCY must be an integer")?
                .max(1),
            None => DEFAULT_CONCURRENCY,
        };
        let toolbox = Self::new(
            pool,
            Options {
                transport,
                client: Client::standalone(),
                models,
                deps_config: config.deps_config(),
                vectors,
                permits: Arc::new(Semaphore::new(concurrency)),
                judge_quota: None,
                history_len: DEFAULT_HISTORY,
                offer: config.source_offer().clone(),
                operator: config.operator().clone(),
            },
        );
        Ok(Local {
            toolbox,
            admin: admin::Admin::grant(),
        })
    }

    /// What this tool says about the instance's release, read again once
    /// [`release::CHECK_EVERY`] has passed; `None` inside the instance.
    pub async fn release_notice(&self) -> Option<release::Notice> {
        self.db.notice().await
    }

    /// [`Self::release_notice`] as last read, without reading the database.
    #[must_use]
    pub fn release_notice_cached(&self) -> Option<release::Notice> {
        self.db.notice_cached()
    }

    /// Whether the built-in pipeline can run.
    #[must_use]
    pub fn has_pipeline(&self) -> bool {
        self.db.has_pipeline()
    }
}

#[cfg(test)]
mod meter_tests {
    use super::*;

    #[test]
    fn the_window_resets_and_the_limit_holds() {
        let start = Instant::now();
        let mut m = Meter {
            quota: Quota {
                limit: 2,
                window: Duration::from_mins(1),
            },
            started: start,
            count: 0,
        };
        assert!(m.allow(start));
        assert!(m.allow(start + Duration::from_secs(1)));
        assert!(
            !m.allow(start + Duration::from_secs(2)),
            "third run in the window"
        );
        assert!(m.allow(start + Duration::from_mins(1)), "a new window");
        assert!(m.allow(start + Duration::from_secs(61)));
        assert!(!m.allow(start + Duration::from_secs(62)));
    }
}
