//! The data steps behind `judge-ingest`: Scryfall sync ([`scryfall`]), the
//! Comprehensive Rules loader ([`cr`]), the curated lists ([`aliases`],
//! [`notes`]), embedding ([`embed`], [`reembed`]), the Discord emoji upload
//! ([`emoji`]) and the explicit migration ([`schema`]), plus the two
//! sequences built from them: [`init`], the first load, and [`refresh`], the
//! scheduled job. `judge-ingest` is argument parsing over this module.
//!
//! They live in the library, beside the other Postgres adapters, so that a
//! long-running binary can run them too: `judge-ingest` depends on this crate,
//! so this crate's binaries could not depend on `judge-ingest`.
//!
//! The embedder comes from `judge.toml` / `VOYAGE_API_KEY` through
//! [`crate::config`], the same loader the bot uses, so `embed` writes the space
//! the bot queries. `emoji` needs no database at all, only `DISCORD_TOKEN`.
//! Downloads are cached under [`cache_dir`].

pub mod aliases;
pub mod cr;
pub mod embed;
pub mod emoji;
pub mod notes;
pub mod reembed;
mod renumber;
pub mod schema;
pub mod scryfall;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context as _, Result};
use judge_embed::WithSpace;
use sqlx::PgPool;

/// Default download cache, relative to the working directory (gitignored).
pub const DEFAULT_CACHE_DIR: &str = ".cache";

/// The download cache: `INGEST_CACHE_DIR`, else [`DEFAULT_CACHE_DIR`].
#[must_use]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("INGEST_CACHE_DIR")
        .map_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR), PathBuf::from)
}

/// A pool on `DATABASE_URL` for the ingest steps.
///
/// # Errors
/// When `DATABASE_URL` is unset or the database cannot be reached.
pub async fn connect() -> Result<PgPool> {
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
///
/// # Errors
/// When the configuration does not load or its embedder cannot be built.
pub fn embedder_from_config() -> Result<Option<Arc<dyn WithSpace>>> {
    let config = crate::config::Config::load().context("loading the model configuration")?;
    tracing::info!("{}", config.summary());
    let embedder = config.embedder()?;
    if embedder.is_none() {
        tracing::warn!(
            "no embedder configured (VOYAGE_API_KEY or [models.embed]); embedding steps will be skipped"
        );
    }
    Ok(embedder)
}

/// The whole first load, in dependency order: the schema, the cards the
/// curated lists name, the rules, then the vectors over both and the emoji.
///
/// Unlike [`refresh`] it stops at the first failure, because each step needs
/// the one before it (aliases resolve against cards, embeddings read rules).
/// Every step is idempotent, so the fix for a failed `init` is to run it
/// again. It loads the built-in alias and note lists, replacing the tables,
/// so an operator who keeps their own list loads that afterwards. With no
/// embedder configured `embed` skips itself with a warning, and with no
/// `DISCORD_TOKEN` the emoji step is skipped the same way.
///
/// # Errors
/// The first step that failed, named.
pub async fn init(pool: &PgPool, cache_dir: &Path) -> Result<()> {
    let started = Instant::now();
    let mut n = 0u8;
    let mut begin = |name: &'static str| {
        n = n.saturating_add(1);
        tracing::info!(step = name, "init step {n} of 7");
        Instant::now()
    };
    let done = |name: &'static str, t: Instant| {
        tracing::info!(step = name, secs = t.elapsed().as_secs(), "init step ok");
    };

    // Before the download: a judge.toml that does not load should fail in a
    // second, not at step 6.
    let embedder = embedder_from_config().context("init: the model configuration")?;

    let t = begin("migrate");
    schema::migrate(pool).await.context("init: migrate")?;
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
    let holds_vectors = crate::db::space::stored_counts(pool)
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
///
/// # Errors
/// When any step failed, naming each one.
pub async fn refresh(pool: &PgPool, cache_dir: &Path) -> Result<()> {
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
        crate::db::retire_unsupported(pool)
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

    /// `init` loads these with no file to fall back on, so a list that stopped
    /// parsing must fail here rather than on an operator's first run.
    #[test]
    fn the_built_in_lists_parse() -> Result<()> {
        assert!(!scryfall::parse_alias_yaml(aliases::BUILTIN)?.is_empty());
        assert!(!notes::parse_notes_yaml(notes::BUILTIN)?.is_empty());
        Ok(())
    }
}
