//! `sqlx::migrate!` embeds each file under `migrations/` with `include_str!`,
//! which tracks the files that exist at build time, not the directory: a
//! *new* migration would not recompile `judge-bot`, and a locally built
//! `judge-ingest migrate` (or the `#[sqlx::test]` suites) would carry a stale
//! set and report "schema is current" with one pending. This is what
//! `sqlx migrate build-script` generates; keep it beside `MIGRATOR`.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
