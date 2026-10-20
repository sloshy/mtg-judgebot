//! `judgebot` — the one long-running binary. What a process does is chosen at
//! launch as a set of roles ([`roles::Role`]): `--discord` (the bot),
//! `--api`, `--web` and `--mcp` (the HTTP interfaces, on one listener),
//! `--admin` (the operator's pages and reads, on a loopback listener of its
//! own) and `--jobs` (the scheduled data refresh). `judgebot ingest <cmd>` is the data
//! command line ([`ingest`]), `judgebot backup <cmd>` the database backup
//! ([`backup`], the `backup` compose service); [`cli`] parses the command
//! line.
//!
//! One process, one composition: one pool, one configuration, one set of
//! models behind one spend meter and one ledger, one `Vectors`, the schema
//! migrated once. Every role's requirements are checked ([`roles::plan`])
//! before anything connects or binds, and the serving roles run concurrently:
//! the first to stop ends the process, non-zero, so the restart policy brings
//! the whole of it back.
//!
//! SIGTERM (`docker stop`, a deploy) or Ctrl-C requests a graceful shutdown
//! ([`judge_bot::shutdown`]): the roles take no new work, the Discord role
//! releases the gateway lease, the answers in flight are finished within
//! [`DRAIN_LIMIT`], the spend ledger is written, and the process exits 0. A
//! signal before the roles start, or a second one, exits at once.
//!
//! Environment (a `.env` in the working directory is loaded first; the
//! process environment wins): `JUDGE_ROLES` when the command line names no
//! role (a set `API_INTERFACES`, retired, refuses the launch); `DATABASE_URL`;
//! the model setup (`JUDGE_CONFIG` or a `./judge.toml`, else `ANTHROPIC_API_KEY` and `VOYAGE_API_KEY`); `JUDGE_MAX_USD`,
//! `JUDGE_BUDGET_PERIOD`, `JUDGE_ALERT_WEBHOOK`; `JUDGE_REFRESH_HOURS` and
//! `INGEST_CACHE_DIR` under `--jobs`; `DISCORD_TOKEN`, `GUILD_ID`,
//! `JUDGE_ROLE`, `JUDGE_CONCURRENCY` and `JUDGE_USER_*` under `--discord`;
//! `API_*`, `WEB_DIST` and `MCP_*` under the network roles; `ADMIN_ADDR`,
//! `ADMIN_TOKEN` and `WEB_DIST` under `--admin`; `RUST_LOG`.

mod backup;
mod cli;
mod ingest;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, Result};
use judge_api::admin::AdminListening;
use judge_bot::{
    config::Config as JudgeConfig,
    db::migrate::Upgrade,
    discord::{Delivery, endpoint::Interactions},
    lease::SchemaHold,
    maintenance::Availability,
    serving::Serving,
    shutdown::{DRAIN_LIMIT, Shutdown},
};

use cli::Invocation;
use judgebot::roles::{self, Adapters, Plan, ROLES_ENV, Roles, Serve};

#[tokio::main]
async fn main() -> Result<()> {
    // The command line before anything else: `--help` and a typo'd flag must
    // not need a database, a key or a working directory to answer.
    match cli::parse(std::env::args_os().skip(1))? {
        Invocation::Help(usage) => {
            println!("{usage}");
            Ok(())
        }
        Invocation::Ingest { args } => {
            let cmd = match ingest::parse(args)? {
                ingest::Launch::Help => {
                    println!("{}", ingest::USAGE);
                    return Ok(());
                }
                ingest::Launch::Run(cmd) => cmd,
            };
            setup()?;
            ingest::run(cmd).await
        }
        Invocation::Backup { args } => match backup::parse(args)? {
            backup::Launch::Help => {
                println!("{}", backup::USAGE);
                Ok(())
            }
            backup::Launch::Run(cmd) => {
                backup::setup()?;
                backup::run(cmd).await
            }
        },
        Invocation::Serve { roles } => {
            setup()?;
            let judge_roles = std::env::var(ROLES_ENV).ok();
            // A 1.x `.env` may still choose interfaces through API_INTERFACES,
            // which the compose file no longer reads: starting without the
            // roles it named (--mcp, say) would drop them silently.
            roles::refuse_api_interfaces(
                std::env::var(roles::API_INTERFACES_ENV).ok().as_deref(),
                judge_roles.as_deref(),
                roles.is_some(),
            )?;
            let (roles, origin) = cli::resolve(roles, judge_roles.as_deref())?;
            tracing::info!(roles = %roles, off = %roles.off(), from = %origin, "judgebot roles");
            // Boxed: the roles' futures together are too large for the stack frame.
            Box::pin(serve(&roles)).await
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

/// What the process calls itself in the spend alerts, the refresh record
/// and its database connections.
const PROCESS: &str = "judgebot";

async fn serve(roles: &Roles) -> Result<()> {
    let process = PROCESS;
    // The signals first: as PID 1 in a container, a process without a
    // handler ignores SIGTERM, so a stop during a slow startup (a long
    // migration, an unreachable database) would wait for SIGKILL.
    let shutdown = Shutdown::new(DRAIN_LIMIT);
    let started = Arc::new(AtomicBool::new(false));
    listen(&shutdown, &started)?;
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
    // The schema first. A migration that only adds (within a major) runs
    // here, beside any previous release still answering, and the process
    // opens with it. A major upgrade's must wait for the previous release to
    // stop: the roles start closed for maintenance and the migration runs
    // once it can (`major`, below). A failure exits, so the restart policy
    // makes it loud rather than answering without persisting.
    let upgrade = if judge_bot::db::migrate::auto_migrate_enabled()? {
        judge_bot::db::migrate::upgrade(&pool)
            .await
            .context("reading the schema")?
    } else {
        Upgrade::InPlace
    };
    let Plan { serve, admin, jobs } = plan;
    let (availability, mut schema) = match &upgrade {
        Upgrade::InPlace => {
            // The hold first: a process of an older release that starts while
            // a major upgrade waits to migrate queues here, and then finds the
            // migrated schema in `at_startup` and refuses it.
            let hold = SchemaHold::shared(&pool, process).await?;
            judge_bot::db::migrate::at_startup(&pool)
                .await
                .context("migrating the schema")?;
            (Availability::open(), Some(hold))
        }
        Upgrade::Major { from } => {
            tracing::warn!(
                from = %from.as_ref().map_or_else(|| "a release from before 2.0".to_owned(), judge_bot::db::migrate::InstanceRelease::describe),
                "a major upgrade: answering with a maintenance reply until every process of the \
                 previous release has stopped and the schema is migrated"
            );
            (Availability::upgrading(), None)
        }
    };
    // With no serving role there is nobody to tell: migrate before the jobs.
    // From here a signal no longer exits at once: a migration it began is
    // finished, then the process stops.
    if schema.is_none() && serve.is_none() {
        started.store(true, Ordering::Release);
        schema = Some(major(&pool, process, &AtomicBool::new(false)).await?);
        availability.set_open();
    }
    // The admin listener's address, reserved before anything else starts,
    // so a taken ADMIN_ADDR fails first, as API_ADDR does. It listens once
    // the instance is open.
    let admin = match admin {
        Some(a) => Some(a.reserve(
            &pool,
            availability.clone(),
            shutdown.clone(),
            judge.locale(),
        )?),
        None => None,
    };
    // The serving roles' shared composition, which also loads the period's
    // spend before any of them can take a question.
    let serving = match serve {
        Some(Serve { adapters, models }) => Some((
            Serving::start(
                pool.clone(),
                &judge,
                models,
                shutdown.clone(),
                availability.clone(),
                process,
            )
            .await?,
            adapters,
        )),
        None => None,
    };
    // The scheduled data refresh, on a thread and a pool of its own
    // (JUDGE_REFRESH_HOURS; 0 leaves it to cron). Its embedding step bills to
    // the process's meter, the one the serving roles' models bill to. Under
    // a major upgrade it starts once the schema is migrated.
    let mut jobs_ledger = None;
    let start_jobs = |jobs| {
        let pool = pool.clone();
        let meter = judge.meter().clone();
        async move { judge_bot::jobs::start(&pool, jobs, meter, process).await }
    };
    let (scheduler, later_jobs) = match jobs {
        Some(jobs) if schema.is_some() => {
            if serving.is_none() {
                // Alone, the jobs still spend (the embed step), so the ledger runs
                // here too: the period's spend caps the refresh, and the refresh's
                // spend reaches `judge-cli stats`. A tripped cap fails the embed
                // step, which the refresh reports itself; the questions alert
                // counts chat refusals, which a process that answers nothing
                // never has. The spend warnings apply as anywhere.
                let budget = judge.budget().clone();
                // It syncs on its own task for the life of the process, and is
                // written once more at a requested shutdown.
                jobs_ledger = Some(
                    judge_bot::budget::start(pool.clone(), judge.meter().clone(), budget, process)
                        .await,
                );
            }
            (start_jobs(jobs).await, None)
        }
        Some(jobs) => (None, Some(jobs)),
        None => {
            tracing::info!(
                "scheduled data refresh: not this process's role (add --jobs to run it here)"
            );
            (None, None)
        }
    };
    // From here on a signal drains the roles instead of ending the process.
    started.store(true, Ordering::Release);
    match serving {
        // Beside serving roles a dead scheduler thread is logged at ERROR by
        // the thread itself and the process goes on answering: questions
        // are worth more than a refresh, which `judgebot ingest refresh`
        // or the next restart provides. The handle is dropped.
        // A refresh in progress at a shutdown ends with the process; its
        // run record is left open, as after a crash.
        Some((serving, adapters)) => {
            let roles = Box::pin(beside_admin(&shutdown, run(&serving, adapters), admin));
            tokio::pin!(roles);
            // The schema hold is kept beside the roles: one lost for good ends
            // the process ([`SchemaHold::keep`]).
            let ran = if let Some(hold) = schema.take() {
                tokio::select! {
                    ran = &mut roles => ran,
                    lost = hold.keep(&pool, process) => Err(lost),
                }
            } else {
                // A major upgrade: the roles answer with the maintenance reply
                // while the migration waits its turn, then open.
                let migrating = AtomicBool::new(false);
                let upgrading = async {
                    let hold = major(&pool, process, &migrating).await?;
                    availability.set_open();
                    tracing::info!("schema migrated; answering questions");
                    if let Some(jobs) = later_jobs
                        && !shutdown.is_requested()
                    {
                        // Beside serving roles a scheduler is dropped, as above.
                        drop(start_jobs(jobs).await);
                    }
                    anyhow::Ok(hold)
                };
                tokio::pin!(upgrading);
                tokio::select! {
                    ran = &mut roles => {
                        // A migration under way is finished, never cut off
                        // between two of its migrations: the previous release
                        // has stopped, and only this one can run on what it
                        // leaves.
                        if migrating.load(Ordering::Acquire) {
                            tracing::warn!("stopping during the migration; finishing it first");
                            upgrading.await.context("finishing the migration")?;
                        }
                        ran
                    }
                    hold = &mut upgrading => {
                        let hold = hold?;
                        tokio::select! {
                            ran = &mut roles => ran,
                            lost = hold.keep(&pool, process) => Err(lost),
                        }
                    }
                }
            };
            if shutdown.is_requested() {
                serving.flush().await;
            }
            ran
        }
        // The admin listener is the only role that serves: it runs as the
        // serving roles do, and a scheduler beside it is dropped as beside
        // them (its thread logs an ERROR if it dies).
        None if admin.is_some() => {
            drop(scheduler);
            let keep = async {
                match schema.take() {
                    Some(hold) => hold.keep(&pool, process).await,
                    None => std::future::pending().await,
                }
            };
            let listener = async {
                match admin {
                    Some(a) => {
                        let ran = a.serve().await;
                        if shutdown.is_requested() {
                            ran
                        } else {
                            stopped("the admin listener", ran)
                        }
                    }
                    None => Ok(()),
                }
            };
            let ran = tokio::select! {
                ran = listener => ran,
                lost = keep => Err(lost),
            };
            if shutdown.is_requested()
                && let Some(ledger) = &jobs_ledger
            {
                judge_bot::serving::flush(ledger).await;
            }
            ran
        }
        // The jobs are the whole process: when their thread ends, so does
        // the process, non-zero, so the restart policy brings them back.
        // `plan` refused a jobs-only launch with the schedule off.
        None => match scheduler {
            Some(scheduler) => {
                let keep = async {
                    match schema.take() {
                        Some(hold) => hold.keep(&pool, process).await,
                        None => std::future::pending().await,
                    }
                };
                let ended = tokio::select! {
                    lost = keep => return Err(lost),
                    ended = scheduler.ended() => ended,
                    () = shutdown.requested() => {
                        if let Some(ledger) = &jobs_ledger {
                            judge_bot::serving::flush(ledger).await;
                        }
                        return Ok(());
                    }
                };
                anyhow::bail!(
                    "the scheduler thread {ended}, and it is this process's only role; \
                     exiting so the process restarts"
                )
            }
            None => anyhow::bail!("no role to run: --jobs alone with the schedule off"),
        },
    }
}

/// A major upgrade's migration: wait for the exclusive schema hold (every
/// process of the previous release gone), migrate, and keep the shared side.
/// Another process of this release may migrate first (two started at once):
/// its shared hold would keep the exclusive side from ever being granted, so
/// between attempts the wait checks whether anything is left to migrate, and
/// joins as an ordinary start once nothing is.
async fn major(
    pool: &sqlx::PgPool,
    process: &'static str,
    migrating: &AtomicBool,
) -> Result<SchemaHold> {
    let still_needed = || async {
        match judge_bot::db::migrate::upgrade(pool).await {
            Ok(u) => matches!(u, Upgrade::Major { .. }),
            Err(e) => {
                tracing::warn!(error = %e, "checking whether the schema was migrated");
                true
            }
        }
    };
    if let Some(exclusive) = SchemaHold::exclusive(pool, process, still_needed).await? {
        migrating.store(true, Ordering::Release);
        judge_bot::db::migrate::at_startup(pool)
            .await
            .context("migrating the schema")?;
        exclusive.downgrade().await
    } else {
        tracing::info!("another process of this release migrated the schema; joining it");
        judge_bot::db::migrate::at_startup(pool)
            .await
            .context("checking the migrated schema")?;
        SchemaHold::shared(pool, process).await
    }
}

/// Request `shutdown` on the first SIGTERM or SIGINT, and exit at once on
/// the second. Before the roles have `started` there is nothing to drain,
/// so the first exits at once too: a migration in progress is rolled back
/// with its connection.
fn listen(shutdown: &Shutdown, started: &Arc<AtomicBool>) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let mut int = signal(SignalKind::interrupt()).context("listen for SIGINT")?;
    let (shutdown, started) = (shutdown.clone(), Arc::clone(started));
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let name = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            };
            if !started.load(Ordering::Acquire) {
                tracing::info!(signal = name, "a signal during startup: exiting");
                std::process::exit(0);
            }
            if first {
                first = false;
                shutdown.request(name);
            } else {
                tracing::warn!(
                    signal = name,
                    "a second signal: exiting without waiting for the drain"
                );
                std::process::exit(1);
            }
        }
    });
    Ok(())
}

/// `roles` beside the admin listener, when there is one: the first to stop
/// ends the process, unless the stop was requested (then both drain).
async fn beside_admin(
    shutdown: &judge_bot::shutdown::Shutdown,
    roles: impl Future<Output = Result<()>>,
    admin: Option<AdminListening>,
) -> Result<()> {
    match admin {
        None => roles.await,
        Some(admin) => {
            together(
                shutdown,
                ("the serving roles", roles),
                ("the admin listener", admin.serve()),
            )
            .await
        }
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
    if let Some(a) = &plan.admin {
        tracing::info!(addr = %a.addr(), "admin listener: `judge-cli admin-url` prints its address");
    }
}

/// Run the serving roles. The HTTP listener is bound first, so a taken
/// `API_ADDR` fails before the Discord gateway is contacted. With
/// `DISCORD_INTERACTIONS=http` the listener also serves Discord's
/// interactions endpoint: the network roles', or one of its own when there
/// are none.
async fn run(serving: &Serving, adapters: Adapters) -> Result<()> {
    // Discord's interactions endpoint, shared by the Discord role (which
    // opens it) and the listener (which serves it); unused over the gateway.
    let interactions = Interactions::new();
    let over_http = |d: &roles::Discord| d.cfg.interactions == Delivery::Http;
    match adapters {
        Adapters::Discord(d) => match d.listen {
            None => judge_bot::discord::serve(serving, d.cfg, d.operator, &interactions).await,
            Some(cfg) => {
                let listening =
                    judge_api::Listening::interactions_only(&cfg, serving, &interactions)?;
                let discord = judge_bot::discord::serve(serving, d.cfg, d.operator, &interactions);
                together(
                    serving.shutdown(),
                    ("the Discord role", discord),
                    ("the HTTP roles", listening.serve()),
                )
                .await
            }
        },
        Adapters::Network(n) => n.bind(serving, None).await?.serve().await,
        Adapters::Both(d, n) => {
            let endpoint = over_http(&d).then_some(&interactions);
            let listening = n.bind(serving, endpoint).await?;
            let discord = judge_bot::discord::serve(serving, d.cfg, d.operator, &interactions);
            together(
                serving.shutdown(),
                ("the Discord role", discord),
                ("the HTTP roles", listening.serve()),
            )
            .await
        }
    }
}

/// Two roles side by side: the Discord role and the HTTP listener, or the
/// serving roles and the admin listener. The first to stop ends the
/// process: a bot whose gateway is gone must not hide behind a page that
/// still answers. A stop the process asked for is the exception: each is
/// draining, so both are waited for.
async fn together(
    shutdown: &judge_bot::shutdown::Shutdown,
    (first_name, first): (&str, impl Future<Output = Result<()>>),
    (second_name, second): (&str, impl Future<Output = Result<()>>),
) -> Result<()> {
    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    tokio::select! {
        r = &mut first => {
            if !shutdown.is_requested() {
                return stopped(first_name, r);
            }
            let other = second.await;
            r.with_context(|| format!("{first_name} failed while shutting down"))?;
            other.with_context(|| format!("{second_name} failed while shutting down"))
        }
        r = &mut second => {
            if !shutdown.is_requested() {
                return stopped(second_name, r);
            }
            let other = first.await;
            r.with_context(|| format!("{second_name} failed while shutting down"))?;
            other.with_context(|| format!("{first_name} failed while shutting down"))
        }
    }
}

/// A role that returned while another was running: an error either way, so
/// the process exits non-zero and is restarted whole.
fn stopped(what: &str, result: Result<()>) -> Result<()> {
    result.with_context(|| format!("{what} failed"))?;
    anyhow::bail!("{what} stopped while the others were running; exiting so the process restarts")
}
