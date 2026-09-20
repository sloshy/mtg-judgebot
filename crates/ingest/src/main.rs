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
    Aliases { yaml: Yaml },
    Notes { yaml: Yaml },
    Init,
    Embed,
    Reembed { yes: bool, clear: bool },
    Emoji,
    Retire,
    Migrate,
    Refresh,
}

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

    fn text(&self, builtin: &'static str) -> Result<std::borrow::Cow<'static, str>> {
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
    match args.next().as_deref() {
        Some("cards") => Ok(Command::Cards),
        Some("rules") => Ok(Command::Rules {
            source: args
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: ingest rules <path-or-url | latest>"))?,
        }),
        Some("aliases") => Ok(Command::Aliases {
            yaml: Yaml::from_arg(args.next()),
        }),
        Some("notes") => Ok(Command::Notes {
            yaml: Yaml::from_arg(args.next()),
        }),
        Some("init") => Ok(Command::Init),
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
        Command::Aliases { yaml } => {
            aliases::run(&connect().await?, &yaml.text(aliases::BUILTIN)?).await
        }
        Command::Notes { yaml } => notes::run(&connect().await?, &yaml.text(notes::BUILTIN)?).await,
        Command::Init => init(&connect().await?, &cache_dir).await,
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

/// The whole first load, in dependency order: the schema, the cards the
/// curated lists name, the rules, then the vectors over both and the emoji.
///
/// Unlike [`refresh`] it stops at the first failure, because each step needs
/// the one before it (aliases resolve against cards, embeddings read rules).
/// Every step is idempotent, so the fix for a failed `init` is to run it
/// again. It loads the built-in alias and note lists, replacing the tables,
/// so an operator who keeps their own list loads that afterwards. With no embedder configured `embed` skips itself with a warning,
/// and with no `DISCORD_TOKEN` the emoji step is skipped the same way.
async fn init(pool: &PgPool, cache_dir: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    let mut n = 0u8;
    let mut begin = |name: &'static str| {
        n = n.saturating_add(1);
        tracing::info!(step = name, "init step {n} of 7");
        std::time::Instant::now()
    };
    let done = |name: &'static str, t: std::time::Instant| {
        tracing::info!(step = name, secs = t.elapsed().as_secs(), "init step ok");
    };

    // Before the download: a judge.toml that does not load should fail in a
    // second, not at step 6.
    let embedder = embedder_from_config().context("init: the model configuration")?;

    let t = begin("migrate");
    migrate::command(pool).await.context("init: migrate")?;
    done("migrate", t);
    let t = begin("cards");
    scryfall::run(pool, cache_dir)
        .await
        .context("init: cards")?;
    done("cards", t);
    let t = begin("rules");
    cr::run_latest(pool, cache_dir)
        .await
        .context("init: rules latest")?;
    done("rules", t);
    let t = begin("aliases");
    aliases::run(pool, aliases::BUILTIN)
        .await
        .context("init: aliases")?;
    done("aliases", t);
    let t = begin("notes");
    notes::run(pool, notes::BUILTIN)
        .await
        .context("init: notes")?;
    done("notes", t);
    let t = begin("embed");
    // A database that holds no vectors has nothing to lose, so it takes the
    // configured embedder's space whatever that is: `reembed` retypes the
    // columns for a width other than the schema's 1024, where `embed` would
    // refuse. One that already holds vectors is only ever filled, and a
    // mismatch there is refused with the way out (`reembed --yes`, which
    // pays for every row and is therefore never implied).
    let holds_vectors = judge_bot::db::space::stored_counts(pool)
        .await?
        .iter()
        .any(|(_, n)| *n > 0);
    match (embedder.as_deref(), holds_vectors) {
        (Some(e), false) => reembed::run(pool, Some(e), true, false)
            .await
            .context("init: embed")?,
        (e, _) => embed::run(pool, e).await.map(drop).context("init: embed")?,
    }
    done("embed", t);
    let t = begin("emoji");
    if std::env::var("DISCORD_TOKEN").is_ok_and(|v| !v.trim().is_empty()) {
        emoji::run(cache_dir).await.context("init: emoji")?;
        done("emoji", t);
    } else {
        tracing::warn!(
            step = "emoji",
            "init step skipped: DISCORD_TOKEN is not set; run `judge-ingest emoji` once the Discord app exists"
        );
    }
    tracing::info!(
        secs = started.elapsed().as_secs(),
        "init done: start the api (`docker compose up -d api`) and ask a question"
    );
    Ok(())
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
            Command::Aliases {
                yaml: Yaml::Builtin
            }
        ));
        assert!(matches!(
            parse(&["notes", "/data/notes.yaml"])?,
            Command::Notes { yaml: Yaml::File(p) } if p == Path::new("/data/notes.yaml")
        ));
        assert!(parse(&["nonsense"]).is_err());
        Ok(())
    }

    /// `init` loads these with no file to fall back on, so a list that stopped
    /// parsing must fail here rather than on an operator's first run.
    #[test]
    fn the_built_in_lists_parse() -> Result<()> {
        assert!(!scryfall::parse_alias_yaml(aliases::BUILTIN)?.is_empty());
        assert!(!notes::parse_notes_yaml(notes::BUILTIN)?.is_empty());
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
