//! The card-symbol emoji the bot renders with: listed from Discord once the
//! bot connects, and listed again while it runs, so new symbols reach a
//! running bot without a restart.
//!
//! [`watch`] does all of it, as a task of its own, so connecting never waits
//! on the database or on Discord: until its first listing lands, symbols
//! render as the literal `{W}`. It reads the run record's mark first and lists
//! the emoji second, so a refresh finishing between the two is caught by the
//! first check. Then every [`CHECK_EVERY`] it reads the record for a run that
//! finished since the last one it saw and may have uploaded emoji
//! ([`runs::emoji_since`]), and lists them again ([`Why`]) when
//!
//! * such a run finished, or the last listing failed: retried on every check
//!   until one succeeds, keeping the table in use meanwhile;
//! * the table is empty: the documented first run starts the bot, then runs
//!   `judgebot ingest emoji` by hand, which the record does not show;
//! * it has not listed them for [`RELIST_EVERY`] checks (an hour): an upload
//!   the record does not show (`judgebot ingest emoji` or `init` by hand, a run
//!   that died mid-upload) or emoji deleted by hand are picked up then.
//!
//! That is one cheap query per check, and a Discord call only on the checks
//! above.

use std::{sync::Arc, time::Duration};

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use super::mana::{SharedSymbols, SymbolTable};
use crate::ingest::runs::{self, EmojiCheck};

/// How often [`watch`] reads the run record.
pub const CHECK_EVERY: Duration = Duration::from_mins(10);
/// After this many checks without a listing, [`watch`] lists the emoji
/// whatever the record says: hourly.
pub const RELIST_EVERY: u32 = 6;
/// The longest one listing of the application's emoji may take: serenity's
/// HTTP client sets no timeout of its own.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest one read of the run record may take.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The application's card-symbol emoji, or `None` (logged) when Discord could
/// not list them. An application with none uploaded is an empty table, and
/// symbols then render as the literal `{W}` Scryfall writes.
async fn load(http: &serenity::Http) -> Option<SymbolTable> {
    match tokio::time::timeout(LIST_TIMEOUT, http.get_application_emojis()).await {
        Ok(Ok(emojis)) => Some(SymbolTable::new(
            emojis.into_iter().map(|e| (e.name, e.id.get())),
        )),
        Ok(Err(e)) => {
            tracing::warn!(
                error = %e,
                "could not list application emoji; keeping the current table, the next check retries"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = LIST_TIMEOUT.as_secs(),
                "listing application emoji timed out; keeping the current table, the next check retries"
            );
            None
        }
    }
}

/// Log what a listing found: the first one always (a warning when there are
/// none), a later one only when the count changed.
fn report(table: &SymbolTable, before: Option<usize>) {
    match before {
        None if table.is_empty() => tracing::warn!(
            "no `{}…` application emoji found; card symbols will render as text \
             (run `judgebot ingest emoji` to upload them)",
            judge_core::symbol::NAME_PREFIX
        ),
        None => tracing::info!(symbols = table.len(), "loaded card-symbol emoji"),
        Some(n) if n != table.len() => {
            tracing::info!(
                before = n,
                symbols = table.len(),
                "card-symbol emoji changed"
            );
        }
        Some(_) => tracing::debug!(symbols = table.len(), "card-symbol emoji unchanged"),
    }
}

/// [`runs::emoji_since`] within [`READ_TIMEOUT`]; `None` (logged) on a
/// failure. A schema without `refresh_runs` (migrations pending) is logged
/// at DEBUG: the startup log already says so.
async fn read(pool: &PgPool, seen: Option<&str>) -> Option<EmojiCheck> {
    match tokio::time::timeout(READ_TIMEOUT, runs::emoji_since(pool, seen)).await {
        Ok(Ok(check)) => Some(check),
        Ok(Err(e)) if runs::is_missing_table(&e) => {
            tracing::debug!(
                error = format_args!("{e:#}"),
                "no run record to watch for emoji"
            );
            None
        }
        Ok(Err(e)) => {
            tracing::warn!(
                error = format_args!("{e:#}"),
                "reading the run record for new emoji"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = READ_TIMEOUT.as_secs(),
                "reading the run record for new emoji: no answer in time"
            );
            None
        }
    }
}

/// Why a check lists the emoji again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    /// A run may have uploaded some, or the last listing failed.
    Stale,
    /// The table is empty.
    Empty,
    /// [`RELIST_EVERY`] checks passed without a listing.
    Due,
}

/// What [`watch`] carries from one check to the next.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Watch {
    /// The latest finished run seen ([`EmojiCheck::latest`]).
    seen: Option<String>,
    /// The table may be out of date: a run may have uploaded emoji since it
    /// was listed, or the listing failed.
    stale: bool,
    /// Checks since the last successful listing.
    since_listed: u32,
}

impl Watch {
    /// Fold one read of the record (`None`: it failed) into the state, and
    /// say whether to list the emoji again. The mark only moves forward: a
    /// read with nothing new keeps it, and a failed read changes nothing.
    fn check(&mut self, read: Option<EmojiCheck>, table_empty: bool) -> Option<Why> {
        self.since_listed = self.since_listed.saturating_add(1);
        if let Some(c) = read {
            if c.uploaded {
                tracing::info!("a data refresh may have uploaded card-symbol emoji");
                self.stale = true;
            }
            if c.latest.is_some() {
                self.seen = c.latest;
            }
        }
        if self.stale {
            Some(Why::Stale)
        } else if table_empty {
            Some(Why::Empty)
        } else if self.since_listed >= RELIST_EVERY {
            Some(Why::Due)
        } else {
            None
        }
    }

    /// A listing succeeded: the table is current.
    fn listed(&mut self) {
        self.stale = false;
        self.since_listed = 0;
    }

    /// A listing failed: retry at the next check.
    fn list_failed(&mut self) {
        self.stale = true;
    }
}

/// The bot's emoji for its whole life: read the record's mark, list the
/// emoji, then every [`CHECK_EVERY`] list them again when [`Watch::check`]
/// says to, replacing `symbols` with what Discord answers.
pub async fn watch(http: Arc<serenity::Http>, pool: PgPool, symbols: Arc<SharedSymbols>) {
    // The mark first: a run finishing after it is caught by the first check.
    // `None` (no run, or unreadable) makes the first check read every run.
    let seen = read(&pool, None).await.and_then(|c| c.latest);
    let mut watching = Watch {
        seen,
        ..Watch::default()
    };
    let mut before = None;
    let mut why = Some(Why::Stale);
    loop {
        if why.is_some() {
            if let Some(table) = load(&http).await {
                // A table that stayed empty was warned about once already.
                if !(why == Some(Why::Empty) && table.is_empty()) {
                    report(&table, before);
                }
                before = Some(table.len());
                symbols.set(table);
                watching.listed();
            } else {
                watching.list_failed();
            }
        }
        tokio::time::sleep(CHECK_EVERY).await;
        let read = read(&pool, watching.seen.as_deref()).await;
        why = watching.check(read, symbols.get().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(latest: Option<&str>, uploaded: bool) -> EmojiCheck {
        EmojiCheck {
            latest: latest.map(str::to_owned),
            uploaded,
        }
    }

    #[test]
    fn a_run_that_uploaded_lists_them_and_moves_the_mark() {
        let mut w = Watch::default();
        assert_eq!(w.check(Some(read(None, false)), false), None, "nothing new");
        assert_eq!(w.check(Some(read(Some("t1"), false)), false), None);
        assert_eq!(w.seen.as_deref(), Some("t1"));
        assert_eq!(
            w.check(Some(read(Some("t2"), true)), false),
            Some(Why::Stale)
        );
        assert_eq!(w.seen.as_deref(), Some("t2"));
    }

    #[test]
    fn a_stale_table_is_listed_on_every_check_until_a_listing_succeeds() {
        let mut w = Watch {
            seen: Some("t1".to_owned()),
            ..Watch::default()
        };
        w.list_failed();
        // The record unreadable, then nothing new: still stale, mark kept.
        assert_eq!(w.check(None, false), Some(Why::Stale));
        assert_eq!(w.check(Some(read(None, false)), false), Some(Why::Stale));
        assert_eq!(w.seen.as_deref(), Some("t1"));
        w.listed();
        assert_eq!(w.check(Some(read(None, false)), false), None);
    }

    #[test]
    fn an_empty_table_is_listed_until_it_is_not() {
        let mut w = Watch::default();
        assert_eq!(w.check(None, true), Some(Why::Empty));
        assert_eq!(
            w.check(Some(read(Some("t1"), false)), true),
            Some(Why::Empty)
        );
        // An upload outranks emptiness: the listing is then news, not a poll.
        assert_eq!(
            w.check(Some(read(Some("t2"), true)), true),
            Some(Why::Stale)
        );
    }

    /// Whatever the record says, the emoji are listed every
    /// [`RELIST_EVERY`] checks; a listing for any reason resets the count.
    #[test]
    fn the_emoji_are_listed_hourly_whatever_the_record_says() {
        let mut w = Watch::default();
        for _ in 1..RELIST_EVERY {
            assert_eq!(w.check(None, false), None);
        }
        assert_eq!(w.check(None, false), Some(Why::Due));
        w.listed();
        assert_eq!(
            w.check(Some(read(Some("t1"), true)), false),
            Some(Why::Stale)
        );
        w.listed();
        for _ in 1..RELIST_EVERY {
            assert_eq!(w.check(Some(read(None, false)), false), None);
        }
        assert_eq!(w.check(Some(read(None, false)), false), Some(Why::Due));
    }
}
