//! `ingest` — Scryfall bulk sync, CR parser, alias loader, embedding. Build-order step 2.
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
//! is non-zero if any step failed, so the scheduler's failure hook fires.
//!
//! `DATABASE_URL` is read from the environment (a `.env` file is honoured); the
//! embedder comes from `judge.toml` / `VOYAGE_API_KEY` through `judge_bot::config`,
//! the same loader the bot uses, so `embed` writes the space the bot queries.
//! `emoji` needs no database at all, only `DISCORD_TOKEN`.
//! Downloads are cached under `INGEST_CACHE_DIR` (default `.cache/`).

mod aliases;
mod cr;
mod embed;
mod emoji;
mod migrate;
mod notes;
mod reembed;
mod renumber;
mod scryfall;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use judge_embed::WithSpace;
use sqlx::PgPool;

/// Default download cache, relative to the working directory (gitignored).
const DEFAULT_CACHE_DIR: &str = ".cache";

#[derive(Debug)]
enum Command {
    Cards,
    Rules { source: String },
    Aliases { path: PathBuf },
    Notes { path: PathBuf },
    Embed,
    Reembed { yes: bool, clear: bool },
    Emoji,
    Retire,
    Migrate,
    Refresh,
}

/// The `rules` argument that means "whatever Wizards currently publishes".
const LATEST: &str = "latest";

const USAGE: &str = "usage: ingest <cards | rules <path-or-url | latest> | aliases <yaml> | notes <yaml> | embed | reembed [--yes] [--clear] | emoji | retire | migrate | refresh>";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    match args.next().as_deref() {
        Some("cards") => Ok(Command::Cards),
        Some("rules") => Ok(Command::Rules {
            source: args
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: ingest rules <path-or-url | latest>"))?,
        }),
        Some("aliases") => Ok(Command::Aliases {
            path: args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("usage: ingest aliases <aliases.yaml>"))?,
        }),
        Some("notes") => Ok(Command::Notes {
            path: args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("usage: ingest notes <notes.yaml>"))?,
        }),
        Some("embed") => Ok(Command::Embed),
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
            Ok(Command::Reembed { yes, clear })
        }
        Some("emoji") => Ok(Command::Emoji),
        Some("retire") => Ok(Command::Retire),
        Some("migrate") => Ok(Command::Migrate),
        Some("refresh") => Ok(Command::Refresh),
        other => anyhow::bail!("{USAGE} (got {other:?})"),
    }
}

async fn connect() -> Result<PgPool> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .context("connecting to DATABASE_URL")
}

/// The configured embedder (`judge.toml`, else `VOYAGE_API_KEY`), or `None`
/// with a warning when neither names one, so an unconfigured environment
/// degrades instead of failing. A configuration that does not load is an
/// error: a typo must not silently skip the embedding step.
fn embedder_from_config() -> Result<Option<Arc<dyn WithSpace>>> {
    let config = judge_bot::config::Config::load().context("loading the model configuration")?;
    tracing::info!("{}", config.summary());
    let embedder = config.embedder()?;
    if embedder.is_none() {
        tracing::warn!(
            "no embedder configured (VOYAGE_API_KEY or [models.embed]); embedding steps will be skipped"
        );
    }
    Ok(embedder)
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
    let cache_dir = std::env::var_os("INGEST_CACHE_DIR")
        .map_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR), PathBuf::from);
    tracing::info!(?cmd, cache_dir = %cache_dir.display(), categories = judge_core::Category::ALL.len(), "ingest");
    // The pool is opened per arm rather than up front: `emoji` talks to
    // Scryfall and Discord only, and must not fail on a missing DATABASE_URL.
    match cmd {
        Command::Cards => scryfall::run(&connect().await?, &cache_dir).await,
        Command::Rules { source } if source == LATEST => {
            cr::run_latest(&connect().await?, &cache_dir)
                .await
                .map(drop)
        }
        Command::Rules { source } => cr::run(&connect().await?, &source, &cache_dir).await,
        Command::Aliases { path } => aliases::run(&connect().await?, &path).await,
        Command::Notes { path } => notes::run(&connect().await?, &path).await,
        Command::Embed => embed::run(&connect().await?, embedder_from_config()?.as_deref())
            .await
            .map(drop),
        Command::Reembed { yes, clear } => {
            reembed::run(
                &connect().await?,
                embedder_from_config()?.as_deref(),
                yes,
                clear,
            )
            .await
        }
        Command::Emoji => emoji::run(&cache_dir).await.map(drop),
        Command::Retire => judge_bot::db::retire_unsupported(&connect().await?)
            .await
            .map(drop)
            .map_err(Into::into),
        Command::Migrate => migrate::command(&connect().await?).await.map(drop),
        Command::Refresh => refresh(&connect().await?, &cache_dir).await,
    }
}

/// Every scheduled step, in dependency order: cards and rules first, then the
/// retirement pass over the calls that cite them, then `embed` so a new CR's rows
/// are embedded in the same run. A failed step is logged and the rest still run;
/// the error names every failure.
async fn refresh(pool: &PgPool, cache_dir: &Path) -> Result<()> {
    let mut failed: Vec<&'static str> = Vec::new();
    let mut step = |name: &'static str, result: Result<()>| match result {
        Ok(()) => tracing::info!(step = name, "refresh step ok"),
        Err(err) => {
            tracing::error!(step = name, error = %format_args!("{err:#}"), "refresh step failed");
            failed.push(name);
        }
    };
    step("cards", scryfall::run(pool, cache_dir).await);
    step("rules", cr::run_latest(pool, cache_dir).await.map(drop));
    step(
        "retire",
        judge_bot::db::retire_unsupported(pool)
            .await
            .map(drop)
            .map_err(Into::into),
    );
    step(
        "embed",
        match embedder_from_config() {
            Ok(embedder) => embed::run(pool, embedder.as_deref()).await.map(drop),
            Err(e) => Err(e),
        },
    );
    // The emoji belong to the bot's Discord application; a database-only
    // deployment (no bot) has no token and nothing to upload to.
    if std::env::var("DISCORD_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
        step("emoji", emoji::run(cache_dir).await.map(drop));
    } else {
        tracing::warn!(
            step = "emoji",
            "refresh step skipped: DISCORD_TOKEN is not set"
        );
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "refresh: {} step(s) failed: {}",
            failed.len(),
            failed.join(", ")
        )
    }
}
