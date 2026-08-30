//! `ingest` — Scryfall bulk sync, CR parser, alias loader, embedding. Build-order step 2.
//!
//! ```text
//! ingest cards                 # Scryfall bulk: cards, card_faces, printed_names, rulings
//! ingest rules <path-or-url>   # Comprehensive Rules txt -> rules + glossary
//! ingest aliases <yaml>        # hand-curated nicknames -> card_aliases
//! ingest notes <yaml>          # hand-written nightmare-card notes -> card_notes
//! ingest embed                 # fill NULL embeddings on rules/glossary/calls via Voyage
//! ```
//!
//! `DATABASE_URL` is read from the environment (a `.env` file is honoured).
//! Downloads are cached under `INGEST_CACHE_DIR` (default `.cache/`).

mod aliases;
mod cr;
mod embed;
mod notes;
mod scryfall;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use judge_core::Embedder;
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
}

const USAGE: &str = "usage: ingest <cards | rules <path-or-url> | aliases <yaml> | notes <yaml> | embed>";

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    match args.next().as_deref() {
        Some("cards") => Ok(Command::Cards),
        Some("rules") => Ok(Command::Rules {
            source: args.next().ok_or_else(|| anyhow::anyhow!("usage: ingest rules <path-or-url>"))?,
        }),
        Some("aliases") => Ok(Command::Aliases {
            path: args.next().map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("usage: ingest aliases <aliases.yaml>"))?,
        }),
        Some("notes") => Ok(Command::Notes {
            path: args.next().map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("usage: ingest notes <notes.yaml>"))?,
        }),
        Some("embed") => Ok(Command::Embed),
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

/// The Voyage embedder if `VOYAGE_API_KEY` is set; `None` (with a warning) otherwise,
/// so an unconfigured environment degrades instead of failing.
fn embedder_from_env() -> Option<Box<dyn Embedder>> {
    match judge_embed::VoyageEmbedder::from_env() {
        Ok(e) => Some(Box::new(e)),
        Err(err) => {
            tracing::warn!(%err, "no embedder configured; embedding steps will be skipped");
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // A missing .env is fine; a malformed one is not.
    match dotenvy::dotenv() {
        Ok(_) | Err(dotenvy::Error::Io(_)) => {}
        Err(err) => return Err(err).context("reading .env"),
    }
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let cmd = parse_args(std::env::args().skip(1))?;
    let cache_dir = std::env::var_os("INGEST_CACHE_DIR").map_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR), PathBuf::from);
    tracing::info!(?cmd, cache_dir = %cache_dir.display(), categories = judge_core::Category::ALL.len(), "ingest");
    let pool = connect().await?;
    match cmd {
        Command::Cards => scryfall::run(&pool, &cache_dir).await,
        Command::Rules { source } => cr::run(&pool, &source, &cache_dir).await,
        Command::Aliases { path } => aliases::run(&pool, &path).await,
        Command::Notes { path } => notes::run(&pool, &path).await,
        Command::Embed => embed::run(&pool, embedder_from_env().as_deref()).await,
    }
}
