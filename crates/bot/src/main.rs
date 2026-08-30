//! `bot` — composition root: Postgres adapters, Voyage embedder, Anthropic
//! synthesizer, Discord (serenity/poise). Build-order step 6.

mod extract;
mod synth;
mod voyage;

use std::sync::Arc;

use anyhow::Result;
use judge_bot::db::{PgCallStore, PgResolver, PgRetriever};
use judge_core::{CallStore, Deps, Embedder, Retriever, Synthesizer};

const SYSTEM_PROMPT: &str = "You are a Magic: The Gathering rules judge. Answer using only the provided \
Comprehensive Rules, rulings and notes. Every citation must quote its source verbatim. Prior calls are \
examples only; the Comprehensive Rules always outrank them.";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let database_url = std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is not set"))?;
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&database_url).await?;
    // One HTTP client (connection pool) shared by every Anthropic adapter.
    let anthropic = judge_anthropic::Client::from_env()?;
    let embedder: Arc<dyn Embedder> = Arc::new(voyage::VoyageEmbedder::from_env()?);
    // The store is handed to the Discord adapter (rating buttons) once it exists.
    let _store: Arc<dyn CallStore> = Arc::new(PgCallStore::new(pool.clone()).with_embedder(Arc::clone(&embedder)));
    let retriever: Arc<dyn Retriever> = Arc::new(PgRetriever::new(pool.clone()).with_embedder(embedder));
    let synthesizer: Arc<dyn Synthesizer> = Arc::new(synth::AnthropicSynthesizer {
        client: anthropic.clone(),
        cfg: judge_anthropic::SynthConfig::default(),
        retriever: Arc::clone(&retriever),
        system_prompt: SYSTEM_PROMPT.to_owned(),
    });
    let _deps = Deps {
        extractor: Arc::new(extract::AnthropicExtractor { client: anthropic }),
        resolver: Arc::new(PgResolver::new(pool)),
        retriever,
        synthesizer,
    };
    todo!("start serenity/poise")
}
