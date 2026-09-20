//! Hand-curated nickname loader: YAML (`alias: Card Name`) -> `card_aliases` (lowercased).
//! The implementation lives in [`crate::scryfall::load_aliases`] (shares the card-name
//! resolution with the Scryfall sync).

use sqlx::PgPool;

/// `data/aliases.yaml` as of this build. The list is a few kilobytes of repo
/// data, so the binary carries it: a first load needs no checkout, and the
/// image needs no `data/` directory.
pub const BUILTIN: &str = include_str!("../../../data/aliases.yaml");

/// Load aliases from YAML `text`, replacing the table contents. Unresolved
/// card names are reported as warnings and skipped.
///
/// # Errors
/// On parse or database failure.
pub async fn run(pool: &PgPool, text: &str) -> anyhow::Result<()> {
    crate::scryfall::load_aliases(pool, text).await
}
