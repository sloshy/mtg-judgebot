//! `ingest` — Scryfall bulk sync, CR parser, embedding. Build-order step 2.

use anyhow::Result;

#[derive(Debug)]
enum Command {
    Cards,
    Rules { path: String },
    Embed,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    match args.next().as_deref() {
        Some("cards") => Ok(Command::Cards),
        Some("rules") => Ok(Command::Rules {
            path: args.next().ok_or_else(|| anyhow::anyhow!("usage: ingest rules <MagicCompRules.txt>"))?,
        }),
        Some("embed") => Ok(Command::Embed),
        other => anyhow::bail!("usage: ingest <cards|rules <path>|embed> (got {other:?})"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let cmd = parse_args(std::env::args().skip(1))?;
    tracing::info!(?cmd, categories = judge_core::Category::ALL.len(), "ingest");
    match cmd {
        Command::Cards => todo!("scryfall oracle-cards + default-cards sync"),
        Command::Rules { path } => todo!("parse CR at {path} into RuleChunk rows"),
        Command::Embed => todo!("embed rules/glossary via Embedder"),
    }
}
