//! `judgebot ingest` — Scryfall bulk sync, CR parser, alias loader, embedding:
//! argument parsing over the steps in `judge_bot::ingest`, where they live so
//! that the long-running roles can run them too.
//!
//! ```text
//! judgebot ingest cards                 # Scryfall bulk: cards, card_faces, printed_names, rulings
//! judgebot ingest rules <path-or-url>   # Comprehensive Rules txt -> rules + glossary
//! judgebot ingest rules latest          # the release linked from Wizards' rules page, if newer than the DB
//! judgebot ingest aliases [yaml]        # hand-curated nicknames -> card_aliases (no file: the built-in copy)
//! judgebot ingest notes [yaml]          # hand-written nightmare-card notes -> card_notes (likewise)
//! judgebot ingest embed                 # fill NULL embeddings on rules/glossary/calls via the configured embedder
//! judgebot ingest reembed [--yes] [--clear]  # make the database hold the configured embedder's space: switch
//!                                       #   and re-embed all when it holds another, else fill what is empty
//!                                       #   (--clear: redo all)
//! judgebot ingest emoji                 # Scryfall card symbols -> the bot's Discord application emoji
//! judgebot ingest retire                # retire/restore calls by whether their citations still hold
//! judgebot ingest migrate               # apply the embedded schema migrations (the serving roles do this at
//!                                       #   startup; this is for an empty database, or JUDGE_AUTO_MIGRATE=false)
//! judgebot ingest refresh               # cards, rules latest, lists, retire, embed, emoji — the scheduled job
//! ```
//!
//! `refresh` is what an operator's own cron runs (`scripts/refresh-data.sh`,
//! docs/DEPLOYMENT.md). Every step is idempotent and each runs even if an earlier
//! one failed — a Scryfall outage must not delay a CR release — and the exit status
//! is non-zero if any step failed, so the scheduler's failure hook fires. The run
//! is recorded in `refresh_runs` as `manual`.
//!
//! Every command that writes data first takes the refresh lease (a database
//! advisory lock, `judge_bot::lease`), waiting for a run in progress, so
//! a manual step or a cron run never overlaps another; [`Leased`] lists them.
//! `init` takes it itself after migrating; `migrate` has its own lock; `emoji`
//! writes no database.
//!
//! `DATABASE_URL` is read from the environment (a `.env` file is honoured); the
//! embedder comes from `judge.toml` / `VOYAGE_API_KEY` through `judge_bot::config`,
//! the same loader the serving roles use, so `embed` writes the space they query.
//!
//! Every embedding is behind the spend cap: the command makes one meter from
//! `JUDGE_MAX_USD` and runs the spend ledger on it (`judge_bot::budget`, with
//! `JUDGE_BUDGET_PERIOD`) like a serving process, so a re-embed counts toward
//! the period the bot is capped by and shows in `judge-cli stats`. The ledger
//! is written once more as the command exits. A run larger than what is left
//! fails at the cap with what it embedded kept; `JUDGE_MAX_USD=… judgebot
//! ingest reembed --yes` raises the cap for that run alone.
//! `emoji` needs no database at all, only `DISCORD_TOKEN`.
//! Downloads are cached under `INGEST_CACHE_DIR` (default `.cache/`).

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use judge_bot::budget::{self, Budget};
use judge_bot::ingest::{
    Embedder, RefreshLease, aliases, cache_dir, connect, cr, embed, embedder_from_config, emoji,
    ensure_writable, init, lease, lists::ListText, notes, reembed, refresh, retire, runs::Trigger,
    schema, scryfall,
};
use judge_llm::SpendMeter;
use sqlx::PgPool;

#[derive(Debug)]
pub enum Command {
    /// Migrates under its own lock, then takes the lease for the rest.
    Init,
    /// Its own lock, and runs before any table exists.
    Migrate,
    /// Scryfall and Discord only; must work with no `DATABASE_URL`.
    Emoji,
    /// Writes data: runs under the refresh lease.
    Leased(Leased),
}

/// The commands that write data, each run under the refresh lease.
#[derive(Debug)]
pub enum Leased {
    Cards,
    Rules {
        source: String,
    },
    Aliases {
        yaml: Yaml,
    },
    Notes {
        yaml: Yaml,
    },
    Embed,
    /// The dry run too, to keep it simple: it waits for a run in progress
    /// rather than report counts that run is changing.
    Reembed {
        yes: bool,
        clear: bool,
    },
    Retire,
    Refresh,
}

/// What `refresh_runs.process` says ran a refresh from this command line.
const PROCESS: &str = "ingest";

/// Where a curated list comes from: the copy of `data/*.yaml` this binary was
/// built with, or a file the operator edited. The load records which, so the
/// refresh keeps a built-in copy current and leaves a file alone.
#[derive(Debug, PartialEq, Eq)]
pub enum Yaml {
    Builtin,
    File(PathBuf),
}

impl Yaml {
    fn from_arg(arg: Option<String>) -> Self {
        arg.map_or(Self::Builtin, |p| Self::File(PathBuf::from(p)))
    }

    fn read(&self) -> Result<ListText> {
        match self {
            Self::Builtin => Ok(ListText::Builtin),
            Self::File(path) => std::fs::read_to_string(path)
                .map(ListText::File)
                .with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// The `rules` argument that means "whatever Wizards currently publishes".
const LATEST: &str = "latest";

/// The usage `judgebot ingest --help` prints.
pub const USAGE: &str = "usage: judgebot ingest <init | cards | rules <path-or-url | latest> | aliases [yaml] | notes [yaml] | embed | reembed [--yes] [--clear] | emoji | retire | migrate | refresh>\n\
init: the whole first load (migrate, cards, rules latest, aliases, notes, embed, emoji); safe to run again.\n\
aliases, notes: with no file, the lists this binary was built with (data/*.yaml), which refresh then keeps current; a file is yours, and refresh leaves it alone.";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let leased = |l| Ok(Command::Leased(l));
    match args.next().as_deref() {
        Some("cards") => leased(Leased::Cards),
        Some("rules") => leased(Leased::Rules {
            source: args.next().ok_or_else(|| {
                anyhow::anyhow!("usage: judgebot ingest rules <path-or-url | latest>")
            })?,
        }),
        Some("aliases") => leased(Leased::Aliases {
            yaml: Yaml::from_arg(args.next()),
        }),
        Some("notes") => leased(Leased::Notes {
            yaml: Yaml::from_arg(args.next()),
        }),
        Some("init") => Ok(Command::Init),
        Some("embed") => leased(Leased::Embed),
        Some("reembed") => {
            let (mut yes, mut clear) = (false, false);
            for flag in args {
                match flag.as_str() {
                    "--yes" => yes = true,
                    "--clear" => clear = true,
                    other => anyhow::bail!(
                        "usage: judgebot ingest reembed [--yes] [--clear] (got {other:?}); --yes: do it, not a dry run; \
                         --clear: clear and re-pay every vector even when the database already holds the configured space"
                    ),
                }
            }
            leased(Leased::Reembed { yes, clear })
        }
        Some("emoji") => Ok(Command::Emoji),
        Some("retire") => leased(Leased::Retire),
        Some("migrate") => Ok(Command::Migrate),
        Some("refresh") => leased(Leased::Refresh),
        other => anyhow::bail!("{USAGE} (got {other:?})"),
    }
}

/// What the ingest command line asked for.
#[derive(Debug)]
pub enum Launch {
    /// Print [`USAGE`] and exit successfully.
    Help,
    /// Run this command.
    Run(Command),
}

/// Parse the arguments after `ingest`. Help and a
/// typo need no database, no `.env` and no logging.
///
/// # Errors
/// An argument that is not UTF-8 (a path must not be altered to fit), or one
/// this command line does not take, with the usage.
pub fn parse(args: Vec<OsString>) -> Result<Launch> {
    let args = args
        .into_iter()
        .map(|a| {
            a.into_string().map_err(|a| {
                anyhow::anyhow!("argument {:?} is not UTF-8\n\n{USAGE}", a.to_string_lossy())
            })
        })
        .collect::<Result<Vec<String>>>()?;
    if args
        .first()
        .is_some_and(|a| ["--help", "-h", "help"].contains(&a.as_str()))
    {
        return Ok(Launch::Help);
    }
    parse_args(args.into_iter()).map(Launch::Run)
}

/// Run `cmd`. The caller has loaded `.env` and started logging.
///
/// # Errors
/// The command's own failure; for `refresh`, any step's.
pub async fn run(cmd: Command) -> Result<()> {
    let cache_dir = cache_dir();
    tracing::info!(?cmd, cache_dir = %cache_dir.display(), categories = judge_core::Category::ALL.len(), "ingest");
    // The pool is opened per arm rather than up front: `emoji` talks to
    // Scryfall and Discord only, and must not fail on a missing DATABASE_URL.
    match cmd {
        Command::Init => {
            let pool = connect().await?;
            // The ledger's table first: on an empty database `init` would
            // create it only after the ledger had failed to read it. `init`
            // migrates again, finding nothing to do.
            schema::migrate(&pool).await.context("init: migrate")?;
            let (meter, ledger) = metered(&pool).await?;
            let result = init(&pool, &cache_dir, PROCESS, &meter).await;
            ledger.flush().await;
            result
        }
        Command::Migrate => schema::migrate(&connect().await?).await.map(drop),
        Command::Emoji => emoji::run(&cache_dir).await.map(drop),
        Command::Leased(cmd) => {
            let meter = SpendMeter::from_env()?;
            // Inputs first, so a typo fails now rather than after a wait.
            let job = Job::prepare(cmd, &meter)?;
            let pool = connect().await?;
            let ledger = budget::start(pool.clone(), meter.clone(), budget()?, PROCESS).await;
            // Waits (with a warning) for a refresh or step in progress, so cron
            // and a manual run take turns instead of failing.
            let mut held = lease(&pool, PROCESS).await?;
            let result = job.run(&mut held, &cache_dir, &meter).await;
            held.release().await;
            ledger.flush().await;
            result
        }
    }
}

/// The command's spend cap (`JUDGE_MAX_USD`) and period (`JUDGE_BUDGET_PERIOD`).
/// No alert: the command fails at the cap itself, naming it.
fn budget() -> Result<Budget> {
    Ok(Budget {
        alert: None,
        ..judge_bot::config::budget(|k| std::env::var(k).ok())?
    })
}

/// A meter from `JUDGE_MAX_USD` with the spend ledger running on it.
async fn metered(pool: &PgPool) -> Result<(SpendMeter, budget::Syncing)> {
    let meter = SpendMeter::from_env()?;
    let ledger = budget::start(pool.clone(), meter.clone(), budget()?, PROCESS).await;
    Ok((meter, ledger))
}

/// A [`Leased`] command with its inputs read: the list file, the embedder.
enum Job {
    Cards,
    RulesLatest,
    Rules(String),
    Aliases(ListText),
    Notes(ListText),
    Embed(Option<Embedder>),
    Reembed {
        embedder: Option<Embedder>,
        yes: bool,
        clear: bool,
    },
    Retire,
    Refresh,
}

impl Job {
    /// `meter` is what the embedder bills to.
    fn prepare(cmd: Leased, meter: &SpendMeter) -> Result<Self> {
        Ok(match cmd {
            Leased::Cards => Self::Cards,
            Leased::Rules { source } if source == LATEST => Self::RulesLatest,
            Leased::Rules { source } => Self::Rules(source),
            Leased::Aliases { yaml } => Self::Aliases(yaml.read()?),
            Leased::Notes { yaml } => Self::Notes(yaml.read()?),
            Leased::Embed => Self::Embed(embedder_from_config(meter)?),
            Leased::Reembed { yes, clear } => Self::Reembed {
                embedder: embedder_from_config(meter)?,
                yes,
                clear,
            },
            Leased::Retire => Self::Retire,
            // Its embedder is read at its embed step: a configuration that
            // does not load fails that step, and the others still run.
            Leased::Refresh => Self::Refresh,
        })
    }

    async fn run(
        self,
        lease: &mut RefreshLease,
        cache_dir: &Path,
        meter: &SpendMeter,
    ) -> Result<()> {
        // `refresh` checks before each of its steps; a single step checks once.
        if !matches!(self, Self::Refresh) {
            ensure_writable(lease).await?;
        }
        match self {
            Self::Cards => scryfall::run(lease, cache_dir).await,
            Self::RulesLatest => cr::run_latest(lease, cache_dir).await.map(drop),
            Self::Rules(source) => cr::run(lease, &source, cache_dir).await,
            Self::Aliases(text) => aliases::run(lease, &text).await.map(drop),
            Self::Notes(text) => notes::run(lease, &text).await.map(drop),
            Self::Embed(embedder) => embed::run(lease, embedder.as_deref()).await.map(drop),
            Self::Reembed {
                embedder,
                yes,
                clear,
            } => reembed::run(lease, embedder.as_deref(), yes, clear).await,
            Self::Retire => retire(lease).await.map(drop),
            Self::Refresh => refresh(lease, cache_dir, Trigger::Manual, meter)
                .await
                .ensure_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn the_curated_lists_default_to_the_built_in_copy() -> Result<()> {
        assert!(matches!(parse(&["init"])?, Command::Init));
        assert!(matches!(
            parse(&["aliases"])?,
            Command::Leased(Leased::Aliases {
                yaml: Yaml::Builtin
            })
        ));
        assert!(matches!(
            parse(&["notes", "/data/notes.yaml"])?,
            Command::Leased(Leased::Notes { yaml: Yaml::File(p) }) if p == std::path::Path::new("/data/notes.yaml")
        ));
        assert!(parse(&["nonsense"]).is_err());
        Ok(())
    }

    #[test]
    fn help_is_asked_for_first_and_needs_nothing_else() -> Result<()> {
        for help in ["--help", "-h", "help"] {
            assert!(matches!(
                super::parse(vec![OsString::from(help)])?,
                Launch::Help
            ));
        }
        let r = super::parse(vec![]);
        assert!(
            r.as_ref()
                .is_err_and(|e| format!("{e:#}").contains("usage: judgebot ingest")),
            "{r:?}"
        );
        Ok(())
    }

    /// Every command that writes data parses to [`Leased`], so `main` runs
    /// it under the lease; only these three do not.
    #[test]
    fn only_init_migrate_and_emoji_run_without_the_lease() -> Result<()> {
        for args in [
            &["cards"][..],
            &["rules", "latest"],
            &["aliases"],
            &["notes"],
            &["embed"],
            &["reembed"],
            &["retire"],
            &["refresh"],
        ] {
            assert!(matches!(parse(args)?, Command::Leased(_)), "{args:?}");
        }
        assert!(matches!(parse(&["init"])?, Command::Init));
        assert!(matches!(parse(&["migrate"])?, Command::Migrate));
        assert!(matches!(parse(&["emoji"])?, Command::Emoji));
        Ok(())
    }

    #[test]
    fn a_curated_list_reads_the_built_in_copy_or_the_file() -> Result<()> {
        assert_eq!(Yaml::Builtin.read()?, ListText::Builtin);
        assert!(
            Yaml::File("/nonexistent/aliases.yaml".into())
                .read()
                .is_err()
        );
        Ok(())
    }
}
