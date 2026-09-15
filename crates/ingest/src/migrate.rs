//! `ingest migrate` — apply the schema migrations the binary carries, the
//! explicit form of what `bot` and `api` do at startup
//! (`judge_bot::db::migrate`): for an empty database before the first
//! `docker compose up -d`, and for an operator who set
//! `JUDGE_AUTO_MIGRATE=false`. Reports on stdout, so a terminal shows it
//! without `RUST_LOG`; a database ahead of this binary is an error here,
//! where startup would only warn.

use anyhow::Result;
use judge_bot::db::migrate::{Ahead, Report, run};
use sqlx::PgPool;

/// Apply every pending migration and print what happened.
///
/// # Errors
/// See [`judge_bot::db::migrate::Error`].
pub async fn command(pool: &PgPool) -> Result<Report> {
    println!("migrating (waits for a running refresh or reembed, if any)");
    let r = run(pool, Ahead::Refuse).await?;
    if r.applied.is_empty() {
        println!(
            "schema is current: {} migrations in place, nothing to apply",
            r.already
        );
    } else {
        for v in &r.applied {
            println!("applied {v}");
        }
        println!(
            "applied {} migration(s); {} in place",
            r.applied.len(),
            r.already + r.applied.len()
        );
    }
    Ok(r)
}
