//! `judgebot` — the one long-running binary. What a process does is chosen at
//! launch as a set of roles ([`roles::Role`]): `--discord` (the bot),
//! `--api`, `--web` and `--mcp` (the HTTP interfaces, on one listener) and
//! `--jobs` (the scheduled data refresh). `judgebot ingest <cmd>` is the data
//! command line ([`ingest`]), and the binaries this one replaced answer as
//! links to it ([`cli`]).
//!
//! One process, one composition: one pool, one configuration, one set of
//! models behind one spend meter and one ledger, one `Vectors`, the schema
//! migrated once. Every role's requirements are checked ([`roles::plan`])
//! before anything connects or binds, and the serving roles run concurrently:
//! the first to stop ends the process, non-zero, so the restart policy brings
//! the whole of it back.
//!
//! Environment (a `.env` in the working directory is loaded first; the
//! process environment wins): `JUDGE_ROLES` when the command line names no
//! role; `DATABASE_URL`; the model setup (`JUDGE_CONFIG` or a `./judge.toml`,
//! else `ANTHROPIC_API_KEY` and `VOYAGE_API_KEY`); `JUDGE_MAX_USD`,
//! `JUDGE_BUDGET_PERIOD`, `JUDGE_ALERT_WEBHOOK`; `JUDGE_REFRESH_HOURS` and
//! `INGEST_CACHE_DIR` under `--jobs`; `DISCORD_TOKEN`, `GUILD_ID`,
//! `JUDGE_ROLE`, `JUDGE_CONCURRENCY` and `JUDGE_USER_*` under `--discord`;
//! `API_*`, `WEB_DIST` and `MCP_*` under the network roles; `RUST_LOG`.

mod cli;
mod ingest;
mod roles;

use anyhow::{Context as _, Result};
use judge_bot::{config::Config as JudgeConfig, serving::Serving};

use cli::{Invocation, Name};
use roles::{Adapters, Plan, ROLES_ENV, Roles, Serve};

#[tokio::main]
async fn main() -> Result<()> {
    // The command line before anything else: `--help` and a typo'd flag must
    // not need a database, a key or a working directory to answer.
    let mut argv = std::env::args_os();
    let argv0 = argv.next().unwrap_or_default();
    match cli::parse(&argv0, argv)? {
        Invocation::Help(usage) => {
            println!("{usage}");
            Ok(())
        }
        Invocation::Ingest { args, name } => {
            let cmd = match ingest::parse(args)? {
                ingest::Launch::Help => {
                    println!("{}", ingest::USAGE);
                    return Ok(());
                }
                ingest::Launch::Run(cmd) => cmd,
            };
            setup()?;
            if let Some(w) = cli::compat_warning(name, "judgebot ingest") {
                tracing::warn!("{w}");
            }
            ingest::run(cmd).await
        }
        Invocation::Serve { roles, name } => {
            setup()?;
            let (roles, origin) = cli::resolve(roles, std::env::var(ROLES_ENV).ok().as_deref())?;
            if let Some(w) = cli::compat_warning(name, &format!("judgebot {}", roles.flags())) {
                tracing::warn!("{w}");
            }
            tracing::info!(roles = %roles, off = %roles.off(), from = %origin, "judgebot roles");
            serve(&roles, name).await
        }
    }
}

/// `.env`, then logging.
fn setup() -> Result<()> {
    // A missing .env is fine; a malformed one is not.
    match dotenvy::dotenv() {
        Ok(_) | Err(dotenvy::Error::Io(_)) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("load .env")),
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    Ok(())
}

async fn serve(roles: &Roles, name: Name) -> Result<()> {
    let process = name.process();
    // The models (judge.toml, or the zero-config Anthropic setup; one spend
    // cap for every role) and the operator contacts the roles check.
    let judge = JudgeConfig::load()?;
    tracing::info!("{}", judge.summary());
    // Every role's requirements, the models included, before anything
    // connects or binds: a role the environment cannot satisfy fails here,
    // not as a 404, an open endpoint or a bot that never logs in.
    let plan = roles::plan(roles, &judge, &|k| std::env::var(k).ok())?;
    log_plan(&plan);
    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is not set"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(plan.pool_size())
        .connect(&database_url)
        .await
        .context("connect to Postgres")?;
    // The schema first: pending migrations are applied here (opt out with
    // JUDGE_AUTO_MIGRATE=false), and a failure exits so the restart policy
    // makes it loud rather than answering without persisting.
    judge_bot::db::migrate::at_startup(&pool)
        .await
        .context("migrating the schema")?;
    let Plan { serve, jobs } = plan;
    // The serving roles' shared composition, which also loads the period's
    // spend before any of them can take a question.
    let serving = match serve {
        Some(Serve { adapters, models }) => Some((
            Serving::start(pool.clone(), &judge, models, process).await?,
            adapters,
        )),
        None => None,
    };
    // The scheduled data refresh, on a thread and a pool of its own
    // (JUDGE_REFRESH_HOURS; 0 leaves it to cron).
    let scheduler = if let Some(jobs) = jobs {
        judge_bot::jobs::start(&pool, jobs, process).await
    } else {
        tracing::info!(
            "scheduled data refresh: not this process's role (add --jobs to run it here)"
        );
        None
    };
    match serving {
        // Beside serving roles a dead scheduler thread is logged at ERROR by
        // the thread itself and the process goes on answering: questions
        // are worth more than a refresh, which `judgebot ingest refresh`
        // or the next restart provides. The handle is dropped.
        Some((serving, adapters)) => run(&serving, adapters).await,
        // The jobs are the whole process: when their thread ends, so does
        // the process, non-zero, so the restart policy brings them back.
        // `plan` refused a jobs-only launch with the schedule off.
        None => match scheduler {
            Some(scheduler) => {
                let ended = scheduler.ended().await;
                anyhow::bail!(
                    "the scheduler thread {ended}, and it is this process's only role; \
                     exiting so the process restarts"
                )
            }
            None => anyhow::bail!("no role to run: --jobs alone with the schedule off"),
        },
    }
}

/// The contacts the roles will name, and the network roles' warnings.
fn log_plan(plan: &Plan) {
    let (discord, network) = match plan.serve.as_ref().map(|s| &s.adapters) {
        None => (None, None),
        Some(Adapters::Discord(d)) => (Some(d), None),
        Some(Adapters::Network(n)) => (None, Some(n)),
        Some(Adapters::Both(d, n)) => (Some(d), Some(n)),
    };
    if let Some(d) = discord {
        tracing::info!(discord = %d.operator.username(), "operator contact");
    }
    if let Some(n) = network {
        tracing::info!(email = %n.operator().email(), "operator contact");
        for w in n.warnings() {
            tracing::warn!("{w}");
        }
    }
}

/// Run the serving roles. The HTTP listener is bound first, so a taken
/// `API_ADDR` fails before the Discord gateway is contacted. With both, the
/// first to stop ends the process: a bot whose gateway is gone must not hide
/// behind a page that still answers.
async fn run(serving: &Serving, adapters: Adapters) -> Result<()> {
    match adapters {
        Adapters::Discord(d) => judge_bot::discord::serve(serving, d.cfg, d.operator).await,
        Adapters::Network(n) => n.bind(serving).await?.serve().await,
        Adapters::Both(d, n) => {
            let listening = n.bind(serving).await?;
            tokio::select! {
                r = judge_bot::discord::serve(serving, d.cfg, d.operator) => stopped("the Discord role", r),
                r = listening.serve() => stopped("the HTTP roles", r),
            }
        }
    }
}

/// A role that returned while another was running: an error either way, so
/// the process exits non-zero and is restarted whole.
fn stopped(what: &str, result: Result<()>) -> Result<()> {
    result.with_context(|| format!("{what} failed"))?;
    anyhow::bail!("{what} stopped while the others were running; exiting so the process restarts")
}
