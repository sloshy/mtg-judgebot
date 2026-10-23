//! `ingest migrate` — apply the schema migrations the binary carries, the
//! explicit form of what `judgebot` does at startup
//! ([`crate::db::migrate`]): for an empty database before the first
//! `docker compose up -d`, and for an operator who set
//! `JUDGE_AUTO_MIGRATE=false`. Reports on stdout, so a terminal shows it
//! without `RUST_LOG`; a database ahead of this binary is an error here,
//! where startup would only warn. A major upgrade's migrations
//! ([`crate::db::migrate::Upgrade::Major`]) wait, as at startup, for every
//! serving `judgebot` of the previous release to stop
//! ([`crate::lease::SchemaHold::exclusive`]).

use anyhow::Result;
use sqlx::PgPool;

use crate::{
    db::migrate::{Ahead, Report, Upgrade, run, upgrade},
    lease::SchemaHold,
};

/// Apply every pending migration and print what happened.
///
/// # Errors
/// See [`crate::db::migrate::Error`].
pub async fn migrate(pool: &PgPool) -> Result<Report> {
    // Held until the migration is done; dropped with the function.
    let _exclusive = if matches!(upgrade(pool).await?, Upgrade::Major { .. }) {
        println!(
            "a major upgrade: waiting for every judgebot of the previous release to stop (its name is in the log every 15 s)"
        );
        // Another process (the new judgebot) may migrate first.
        let still_needed =
            || async { matches!(upgrade(pool).await, Ok(Upgrade::Major { .. }) | Err(_)) };
        SchemaHold::exclusive(pool, "judgebot ingest", still_needed).await?
    } else {
        None
    };
    println!(
        "migrating (waits for a CR load, retirement pass or vector write in progress, if any)"
    );
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
    for e in &r.extensions {
        println!("updated extension {} from {} to {}", e.name, e.from, e.to);
    }
    Ok(r)
}
