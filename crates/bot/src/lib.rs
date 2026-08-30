//! `judge-bot` as a library: the sqlx adapters for the DB-backed ports, the
//! Anthropic adapters, and [`build_deps`], the one composition shared by the
//! `bot` and `eval` binaries.

pub mod db;
pub mod discord;
pub mod extract;
pub mod synth;

use std::sync::Arc;

use judge_anthropic::{Client, SynthConfig};
use judge_core::{Deps, Embedder, Retriever};
use sqlx::PgPool;

use db::{PgResolver, PgRetriever};

/// Knobs for [`build_deps_with`]; [`Default`] is what [`build_deps`] uses.
#[derive(Clone, Debug, Default)]
pub struct DepsConfig {
    /// Extraction request knobs: model, effort, output ceiling, history turns.
    pub extract: extract::ExtractConfig,
    /// Synthesis request knobs: model, effort, output ceiling, refusal fallback.
    pub synth: SynthConfig,
    /// Size caps for the synthesizer's user turn.
    pub budget: synth::Budget,
}

/// Wire the Postgres adapters and the Anthropic adapters into [`Deps`] with
/// [`DepsConfig::default`]. `embedder` is optional: without one the retriever
/// skips its vector leg and orders prior calls by recency.
#[must_use]
pub fn build_deps(pool: PgPool, client: Client, embedder: Option<Arc<dyn Embedder>>) -> Deps {
    build_deps_with(pool, client, embedder, &DepsConfig::default())
}

/// [`build_deps`] with explicit configuration.
#[must_use]
pub fn build_deps_with(pool: PgPool, client: Client, embedder: Option<Arc<dyn Embedder>>, cfg: &DepsConfig) -> Deps {
    let mut retriever = PgRetriever::new(pool.clone());
    if let Some(e) = embedder {
        retriever = retriever.with_embedder(e);
    }
    let retriever: Arc<dyn Retriever> = Arc::new(retriever);
    let synthesizer = synth::AnthropicSynthesizer::new(client.clone(), cfg.synth.clone(), Arc::clone(&retriever))
        .with_budget(cfg.budget);
    Deps {
        extractor: Arc::new(extract::AnthropicExtractor::new(client, cfg.extract.clone())),
        resolver: Arc::new(PgResolver::new(pool)),
        retriever,
        synthesizer: Arc::new(synthesizer),
    }
}
