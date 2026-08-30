//! Hand-curated nickname loader: YAML (`alias: Card Name`) -> `card_aliases` (lowercased).
//! The implementation lives in [`crate::scryfall::load_aliases`] (shares the card-name
//! resolution with the Scryfall sync).

use std::path::Path;

use sqlx::PgPool;

/// Load aliases from `path`, replacing the table contents. Unresolved card
/// names are reported as warnings and skipped.
///
/// # Errors
/// On read, parse or database failure.
pub async fn run(pool: &PgPool, path: &Path) -> anyhow::Result<()> {
    crate::scryfall::load_aliases(pool, path).await
}
