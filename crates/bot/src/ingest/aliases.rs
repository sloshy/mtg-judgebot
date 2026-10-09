//! Hand-curated nickname loader: YAML (`alias: Card Name`) -> `card_aliases` (lowercased).
//! The implementation lives in [`super::scryfall::load_aliases`] (shares the card-name
//! resolution with the Scryfall sync).

use super::RefreshLease;

/// `data/aliases.yaml` as of this build. The list is a few kilobytes of repo
/// data, so the binary carries it: a first load needs no checkout, and the
/// image needs no `data/` directory.
pub const BUILTIN: &str = include_str!("../../../../data/aliases.yaml");

/// Load aliases from YAML `text`, replacing the table contents. Unresolved
/// card names are reported as warnings and skipped.
///
/// # Errors
/// On parse or database failure.
pub async fn run(lease: &mut RefreshLease, text: &str) -> anyhow::Result<()> {
    super::scryfall::load_aliases(lease.pool(), text).await
}
