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
//! is non-zero if any step failed, so the scheduler's failure hook fires.
//!
//! `DATABASE_URL` is read from the environment (a `.env` file is honoured); the
//! embedder comes from `judge.toml` / `VOYAGE_API_KEY` through `judge_bot::config`,
//! the same loader the bot uses, so `embed` writes the space the bot queries.
//! `emoji` needs no database at all, only `DISCORD_TOKEN`.
//! Downloads are cached under `INGEST_CACHE_DIR` (default `.cache/`).

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use judge_bot::ingest::{
    aliases, cache_dir, connect, cr, embed, embedder_from_config, emoji, init, notes, reembed,
    refresh, schema, scryfall,
};

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
        Command::Migrate => schema::migrate(&connect().await?).await.map(drop),
        Command::Refresh => refresh(&connect().await?, &cache_dir).await,
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
            Command::Notes { yaml: Yaml::File(p) } if p == std::path::Path::new("/data/notes.yaml")
        ));
        assert!(parse(&["nonsense"]).is_err());
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
