//! `eval` — offline scoring against `eval/gold.yaml`. Needs `DATABASE_URL`
//! (read from the environment or `.env`) and an ingested database; no API key.
//!
//! `eval recall [--vectors] [path/to/gold.yaml]`: for every CR/Commander
//! question, resolves its card names and nicknames, runs the retriever with the
//! gold categories and reports which `expected_rule_ids` the synthesis prompt
//! shows under the production budget (and which were retrieved but cut). No
//! embedder unless `--vectors`, which embeds each question with the configured
//! one (a fraction of a cent for the gold set). Exits non-zero if aggregate
//! retrieved recall is below 90% or shown recall below 75%.
//!
//! `eval rescore <run.json> | answer --label L [--limit N] [--ids a,b] [--max-usd X] [--out p] [--gold p] [--config judge.toml]`:
//! runs the full `judge()` pipeline (live model calls on the configured
//! providers, capped at `--max-usd`, default $2.00) over at most `--limit`
//! (default 2) gold questions and writes `eval/runs/L.json`, recording which
//! model answered each stage.
//!
//! `eval show <run.json>`: expected vs. bot answers side by side.

mod answer;
mod categories;
mod deps;
mod gold;
mod recall;
mod score;

use std::process::ExitCode;

use anyhow::Context as _;

/// The retrieval gate from ARCHITECTURE.md §6 step 4: expected rules retrieved at all.
const RECALL_GATE: f64 = 0.90;
/// Expected rules the synthesis prompt shows under the production budget.
/// 54/67 (81%) on the gold set when this gate was added, from 29/67 (43%)
/// before the legs were ranked. Set a few ids below that on purpose, so a CR
/// update that shifts one rank does not turn it red, while a regression of the
/// kind it was added for (every slot taken by one low-value category) does.
const SHOWN_GATE: f64 = 0.75;

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
    anyhow::anyhow!(
        "usage:\n  eval recall [--vectors] [gold.yaml]\n  eval rescore <run.json> | answer --label <name> [--limit N] [--ids a,b] [--max-usd X] [--out path] [--gold path] [--gold-extraction] [--config judge.toml]\n  eval show <run.json>"
    )
}

/// `Ok(true)` when the run passed its gate.
async fn run() -> anyhow::Result<bool> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().ok_or_else(usage)?;
    match cmd.as_str() {
        "recall" => {
            let mut rest: Vec<String> = args.collect();
            let with_vectors = rest.iter().any(|a| a == "--vectors");
            rest.retain(|a| a != "--vectors");
            let path = rest
                .first()
                .map_or_else(gold::default_path, std::path::PathBuf::from);
            let gold = gold::load(&path)?;
            let pool = connect().await?;
            let vectors = if with_vectors {
                let config = judge_bot::config::Config::load()?;
                Some(
                    config
                        .vectors(pool.clone())?
                        .ok_or_else(|| anyhow::anyhow!("--vectors: no embedder configured"))?,
                )
            } else {
                None
            };
            let report = recall::run(&pool, &gold, vectors).await?;
            print!("{report}");
            Ok(report.retrieved() >= RECALL_GATE && report.recall() >= SHOWN_GATE)
        }
        "answer" => {
            let opts = answer::Options::parse(args)?;
            let pool = connect().await?;
            let run = answer::run(pool, &opts).await?;
            Ok(run.rows.iter().all(|r| r.correct_shape))
        }
        "show" => {
            let path = args.next().ok_or_else(usage)?;
            print!("{}", answer::show(std::path::Path::new(&path))?);
            Ok(true)
        }
        "rescore" => {
            let path = args.next().ok_or_else(usage)?;
            print!(
                "{}",
                answer::rescore(std::path::Path::new(&path), &gold::default_path())?
            );
            Ok(true)
        }
        _ => Err(usage()),
    }
}

async fn connect() -> anyhow::Result<sqlx::PgPool> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .context("connecting to DATABASE_URL")
}
