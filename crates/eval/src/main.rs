//! `eval` — offline scoring against `eval/gold.yaml`. Needs `DATABASE_URL`
//! (read from the environment or `.env`) and an ingested database; no API key.
//!
//! `eval recall [path/to/gold.yaml]`: for every CR/Commander question,
//! resolves its card names and nicknames, runs the retriever with the gold
//! categories (no embedder) and reports which `expected_rule_ids` are in
//! Context. Exits non-zero if aggregate recall is below the 90% gate.

mod categories;
mod gold;
mod recall;

use std::process::ExitCode;

use anyhow::Context as _;

/// The retrieval gate from ARCHITECTURE.md §6 step 4.
const RECALL_GATE: f64 = 0.90;

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> anyhow::Error {
    anyhow::anyhow!("usage: eval recall [gold.yaml]")
}

/// `Ok(true)` when the run passed its gate.
async fn run() -> anyhow::Result<bool> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().ok_or_else(usage)?;
    match cmd.as_str() {
        "recall" => {
            let path = args.next().map_or_else(gold::default_path, std::path::PathBuf::from);
            let gold = gold::load(&path)?;
            let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .connect(&database_url)
                .await
                .context("connecting to DATABASE_URL")?;
            let report = recall::run(&pool, &gold).await?;
            print!("{report}");
            Ok(report.recall() >= RECALL_GATE)
        }
        _ => Err(usage()),
    }
}
