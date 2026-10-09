//! `ingest` — Scryfall bulk sync, CR parser, alias loader, embedding: argument
//! parsing over the steps in `judge_bot::ingest`, where they live so that a
//! long-running binary can run them too.
//!
//! ```text
//! ingest cards                 # Scryfall bulk: cards, card_faces, printed_names, rulings
//! ingest rules <path-or-url>   # Comprehensive Rules txt -> rules + glossary
//! ingest rules latest          # the release linked from Wizards' rules page, if newer than the DB
//! ingest aliases <yaml>        # hand-curated nicknames -> card_aliases
//! ingest notes <yaml>          # hand-written nightmare-card notes -> card_notes
//! ingest embed                 # fill NULL embeddings on rules/glossary/calls via the configured embedder
//! ingest reembed [--yes] [--clear]  # make the database hold the configured embedder's space: switch and
//!                              #   re-embed all when it holds another, else fill what is empty (--clear: redo all)
//! ingest emoji                 # Scryfall card symbols -> the bot's Discord application emoji
//! ingest retire                # retire/restore calls by whether their citations still hold
//! ingest migrate               # apply the embedded schema migrations (bot/api do this at startup;
//!                              #   this is for an empty database, or JUDGE_AUTO_MIGRATE=false)
//! ingest refresh               # cards, rules latest, retire, embed, emoji — the scheduled job
//! ```
//!
//! `refresh` is what the deployment runs unattended (`scripts/refresh-data.sh`,
//! docs/DEPLOYMENT.md). Every step is idempotent and each runs even if an earlier
//! one failed — a Scryfall outage must not delay a CR release — and the exit status
//! is non-zero if any step failed, so the scheduler's failure hook fires. The run
//! is recorded in `refresh_runs` as `manual`.
//!
//! Every command that writes data first takes the refresh lease (a database
//! advisory lock, `judge_bot::ingest::lease`), waiting for a run in progress, so
//! a manual step or a cron run never overlaps another; [`Leased`] lists them.
//! `init` takes it itself after migrating; `migrate` has its own lock; `emoji`
//! writes no database.
//!
//! `DATABASE_URL` is read from the environment (a `.env` file is honoured); the
//! embedder comes from `judge.toml` / `VOYAGE_API_KEY` through `judge_bot::config`,
//! the same loader the bot uses, so `embed` writes the space the bot queries.
//! `emoji` needs no database at all, only `DISCORD_TOKEN`.
//! Downloads are cached under `INGEST_CACHE_DIR` (default `.cache/`).

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use judge_bot::ingest::{
    Embedder, RefreshLease, aliases, cache_dir, connect, cr, embed, embedder_from_config, emoji,
    ensure_writable, init, lease, notes, reembed, refresh, retire, runs::Trigger, schema, scryfall,
};

#[derive(Debug)]
enum Command {
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
enum Leased {
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

/// What `refresh_runs.process` says ran a refresh from this binary.
const PROCESS: &str = "ingest";

/// Where a curated list comes from: the copy of `data/*.yaml` this binary was
/// built with, or a file the operator edited.
#[derive(Debug, PartialEq, Eq)]
enum Yaml {
    Builtin,
    File(PathBuf),
}

impl Yaml {
    fn from_arg(arg: Option<String>) -> Self {
        arg.map_or(Self::Builtin, |p| Self::File(PathBuf::from(p)))
    }

    fn text(&self, builtin: &'static str) -> Result<Cow<'static, str>> {
        match self {
            Self::Builtin => Ok(builtin.into()),
            Self::File(path) => std::fs::read_to_string(path)
                .map(Into::into)
                .with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// The `rules` argument that means "whatever Wizards currently publishes".
const LATEST: &str = "latest";

const USAGE: &str = "usage: ingest <init | cards | rules <path-or-url | latest> | aliases [yaml] | notes [yaml] | embed | reembed [--yes] [--clear] | emoji | retire | migrate | refresh>\n\
init: the whole first load (migrate, cards, rules latest, aliases, notes, embed, emoji); safe to run again.\n\
aliases, notes: with no file, the lists this binary was built with (data/*.yaml).";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let leased = |l| Ok(Command::Leased(l));
    match args.next().as_deref() {
        Some("cards") => leased(Leased::Cards),
        Some("rules") => leased(Leased::Rules {
            source: args
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: ingest rules <path-or-url | latest>"))?,
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
                        "usage: ingest reembed [--yes] [--clear] (got {other:?}); --yes: do it, not a dry run; \
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

#[tokio::main]
async fn main() -> Result<()> {
    // A missing .env is fine; a malformed one is not.
    match dotenvy::dotenv() {
        Ok(_) | Err(dotenvy::Error::Io(_)) => {}
        Err(err) => return Err(err).context("reading .env"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut args = std::env::args().skip(1).peekable();
    if args
        .peek()
        .is_some_and(|a| ["--help", "-h", "help"].contains(&a.as_str()))
    {
        println!("{USAGE}");
        return Ok(());
    }
    let cmd = parse_args(args)?;
    let cache_dir = cache_dir();
    tracing::info!(?cmd, cache_dir = %cache_dir.display(), categories = judge_core::Category::ALL.len(), "ingest");
    // The pool is opened per arm rather than up front: `emoji` talks to
    // Scryfall and Discord only, and must not fail on a missing DATABASE_URL.
    match cmd {
        Command::Init => init(&connect().await?, &cache_dir, PROCESS).await,
        Command::Migrate => schema::migrate(&connect().await?).await.map(drop),
        Command::Emoji => emoji::run(&cache_dir).await.map(drop),
        Command::Leased(cmd) => {
            // Inputs first, so a typo fails now rather than after a wait.
            let job = Job::prepare(cmd)?;
            // Waits (with a warning) for a refresh or step in progress, so cron
            // and a manual run take turns instead of failing.
            let mut held = lease(&connect().await?, PROCESS).await?;
            let result = job.run(&mut held, &cache_dir).await;
            held.release().await;
            result
        }
    }
}

/// A [`Leased`] command with its inputs read: the list file, the embedder.
enum Job {
    Cards,
    RulesLatest,
    Rules(String),
    Aliases(Cow<'static, str>),
    Notes(Cow<'static, str>),
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
    fn prepare(cmd: Leased) -> Result<Self> {
        Ok(match cmd {
            Leased::Cards => Self::Cards,
            Leased::Rules { source } if source == LATEST => Self::RulesLatest,
            Leased::Rules { source } => Self::Rules(source),
            Leased::Aliases { yaml } => Self::Aliases(yaml.text(aliases::BUILTIN)?),
            Leased::Notes { yaml } => Self::Notes(yaml.text(notes::BUILTIN)?),
            Leased::Embed => Self::Embed(embedder_from_config()?),
            Leased::Reembed { yes, clear } => Self::Reembed {
                embedder: embedder_from_config()?,
                yes,
                clear,
            },
            Leased::Retire => Self::Retire,
            // Its embedder is read at its embed step: a configuration that
            // does not load fails that step, and the others still run.
            Leased::Refresh => Self::Refresh,
        })
    }

    async fn run(self, lease: &mut RefreshLease, cache_dir: &Path) -> Result<()> {
        // `refresh` checks before each of its steps; a single step checks once.
        if !matches!(self, Self::Refresh) {
            ensure_writable(lease).await?;
        }
        match self {
            Self::Cards => scryfall::run(lease, cache_dir).await,
            Self::RulesLatest => cr::run_latest(lease, cache_dir).await.map(drop),
            Self::Rules(source) => cr::run(lease, &source, cache_dir).await,
            Self::Aliases(text) => aliases::run(lease, &text).await,
            Self::Notes(text) => notes::run(lease, &text).await,
            Self::Embed(embedder) => embed::run(lease, embedder.as_deref()).await.map(drop),
            Self::Reembed {
                embedder,
                yes,
                clear,
            } => reembed::run(lease, embedder.as_deref(), yes, clear).await,
            Self::Retire => retire(lease).await.map(drop),
            Self::Refresh => refresh(lease, cache_dir, Trigger::Manual).await.ensure_ok(),
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
        assert_eq!(
            Yaml::Builtin.text(aliases::BUILTIN)?.as_ref(),
            aliases::BUILTIN
        );
        assert!(
            Yaml::File("/nonexistent/aliases.yaml".into())
                .text("")
                .is_err()
        );
        Ok(())
    }
}
