//! Format legality: whether a card is legal, banned or restricted in a format.
//!
//! The data is Scryfall's `legalities` object, stored per card by the cards
//! step and refreshed with it. It is the one source for "is this legal in
//! Modern?" and "is this banned in Commander?": the Comprehensive Rules name
//! no card as banned, and the banned lists themselves are not loaded.
//!
//! A card's legality reaches the material as one entry per format,
//! `Modern: banned`, and a [`Citation::Legality`](crate::Citation::Legality)
//! quotes one **whole** entry. A part of one is not a citation: `legal` is a
//! substring of `not legal`, and `Modern` alone says nothing. So the check
//! here is equality with an entry (under the same typographic folding as every
//! other quote), not a substring search over the line.
//!
//! Formats are Scryfall's keys, not an enum: a format Scryfall adds appears in
//! the material at the next refresh with no release of this crate. A key with
//! no name in [`LABELS`] is shown as the key.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CardId, quote};

/// A card's status in one format, as Scryfall states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// Legal.
    Legal,
    /// Not legal: never printed into the format, or rotated out of it.
    NotLegal,
    /// On the format's banned list.
    Banned,
    /// On the format's restricted list. One copy in Vintage; Scryfall uses
    /// the word for other formats' own restrictions too, so the material
    /// states it and does not explain it.
    Restricted,
}

impl Standing {
    /// Scryfall's word for it, or `None` for one this build does not know.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "legal" => Some(Self::Legal),
            "not_legal" => Some(Self::NotLegal),
            "banned" => Some(Self::Banned),
            "restricted" => Some(Self::Restricted),
            _ => None,
        }
    }

    /// How an entry states it.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Legal => "legal",
            Self::NotLegal => "not legal",
            Self::Banned => "banned",
            Self::Restricted => "restricted",
        }
    }
}

/// Scryfall's format keys with the names players use, in the order the
/// material lists them. A key that is not here follows these, by key.
pub const LABELS: &[(&str, &str)] = &[
    ("standard", "Standard"),
    ("pioneer", "Pioneer"),
    ("modern", "Modern"),
    ("legacy", "Legacy"),
    ("vintage", "Vintage"),
    ("commander", "Commander"),
    ("pauper", "Pauper"),
    ("paupercommander", "Pauper Commander"),
    ("oathbreaker", "Oathbreaker"),
    ("duel", "Duel Commander"),
    ("predh", "PreDH"),
    ("premodern", "Premodern"),
    ("oldschool", "Old School"),
    ("brawl", "Brawl"),
    ("standardbrawl", "Standard Brawl"),
    ("historic", "Historic"),
    ("timeless", "Timeless"),
    ("alchemy", "Alchemy"),
    ("gladiator", "Gladiator"),
    ("penny", "Penny Dreadful"),
    ("future", "Future Standard"),
];

/// The longest format key kept. Scryfall's are under twenty characters.
const MAX_KEY_CHARS: usize = 32;

/// The name the material gives the format with this key.
#[must_use]
pub fn label(key: &str) -> &str {
    LABELS
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(key, |(_, name)| name)
}

/// Where a key sorts: its place in [`LABELS`], then the unnamed ones by key.
fn rank(key: &str) -> (usize, &str) {
    (
        LABELS
            .iter()
            .position(|(k, _)| *k == key)
            .unwrap_or(LABELS.len()),
        key,
    )
}

/// One format's entry for a card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FormatStanding {
    /// Scryfall's format key (`modern`, `paupercommander`).
    pub format: String,
    /// The card's status there.
    pub standing: Standing,
}

impl FormatStanding {
    /// The entry as the material prints it and a citation quotes it:
    /// `Modern: banned`.
    #[must_use]
    pub fn entry(&self) -> String {
        format!("{}: {}", label(&self.format), self.standing.text())
    }
}

/// A card's legality in every format Scryfall lists for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CardLegality {
    /// The card.
    pub card: CardId,
    /// One entry per format, in the material's order.
    pub formats: Vec<FormatStanding>,
}

impl CardLegality {
    /// Read Scryfall's `legalities` object. A key that is not a short
    /// lowercase word and a status this build does not know are left out, so
    /// nothing unreadable is shown as a fact. Anything but an object is no
    /// formats at all.
    #[must_use]
    pub fn from_scryfall(card: CardId, legalities: &Value) -> Self {
        let mut formats: Vec<FormatStanding> = legalities
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| {
                !key.is_empty()
                    && key.chars().count() <= MAX_KEY_CHARS
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
            .filter_map(|(key, status)| {
                Some(FormatStanding {
                    format: key.clone(),
                    standing: Standing::parse(status.as_str()?)?,
                })
            })
            .collect();
        formats.sort_by(|a, b| rank(&a.format).cmp(&rank(&b.format)));
        Self { card, formats }
    }

    /// The entry `quote` quotes whole, as this card states it today. `None`
    /// for a part of an entry, an entry of another status, or a format the
    /// card has no entry for.
    #[must_use]
    pub fn locate_entry(&self, quote: &str) -> Option<String> {
        let quote = quote.trim();
        self.formats
            .iter()
            .map(FormatStanding::entry)
            .find(|entry| quote::locate(entry, quote).is_some_and(|span| span.len() == entry.len()))
    }

    /// The card's entry for the format `quote` names, when the quote is not a
    /// whole entry: what a retry notice points at. The format is what stands
    /// before the quote's colon (the whole quote when it has none), and it must
    /// be a format's name or key exactly, case aside. Anything looser names the
    /// wrong entry: `Historic Brawl` holds both `Historic` and `Brawl`.
    #[must_use]
    pub fn near_entry(&self, quote: &str) -> Option<String> {
        // Two entries in one quote name no single format.
        let named = match quote.split_once(':') {
            Some((_, rest)) if rest.contains(':') => return None,
            Some((format, _)) => format,
            None => quote,
        }
        .trim();
        self.formats
            .iter()
            .find(|f| {
                named.eq_ignore_ascii_case(label(&f.format))
                    || named.eq_ignore_ascii_case(&f.format)
            })
            .map(FormatStanding::entry)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn nadu() -> CardLegality {
        CardLegality::from_scryfall(
            CardId::new(Uuid::from_u128(7)),
            &json!({
                "tlr": "restricted", "commander": "banned", "modern": "banned",
                "vintage": "legal", "standard": "not_legal", "paupercommander": "not_legal",
                "pauper": "not_legal", "UPPER": "legal", "newformat": "suspended",
                "": "legal", "legacy": 3,
            }),
        )
    }

    #[test]
    fn entries_are_named_and_ordered() {
        let entries: Vec<String> = nadu().formats.iter().map(FormatStanding::entry).collect();
        assert_eq!(
            entries,
            [
                "Standard: not legal",
                "Modern: banned",
                "Vintage: legal",
                "Commander: banned",
                "Pauper: not legal",
                "Pauper Commander: not legal",
                "tlr: restricted",
            ]
        );
    }

    #[test]
    fn unreadable_keys_and_statuses_are_left_out() {
        let l = nadu();
        assert!(l.formats.iter().all(|f| f.format != "UPPER"));
        assert!(l.formats.iter().all(|f| f.format != "newformat"));
        assert!(l.formats.iter().all(|f| f.format != "legacy"));
        assert!(l.formats.iter().all(|f| !f.format.is_empty()));
        let none = CardLegality::from_scryfall(CardId::new(Uuid::from_u128(7)), &json!([]));
        assert!(none.formats.is_empty());
    }

    #[test]
    fn only_a_whole_entry_is_located() {
        let l = nadu();
        assert_eq!(
            l.locate_entry("Modern: banned").as_deref(),
            Some("Modern: banned")
        );
        assert_eq!(
            l.locate_entry("  Vintage: legal ").as_deref(),
            Some("Vintage: legal")
        );
        for part in ["legal", "Modern", "banned", "Modern: ban", "odern: banned"] {
            assert_eq!(l.locate_entry(part), None, "{part}");
        }
        // Another status, another card's format, and two entries at once.
        assert_eq!(l.locate_entry("Modern: legal"), None);
        assert_eq!(l.locate_entry("Legacy: banned"), None);
        assert_eq!(l.locate_entry("Modern: banned · Vintage: legal"), None);
        // "Standard: not legal" must not pass as "Standard: legal".
        assert_eq!(l.locate_entry("Standard: legal"), None);
        // A longer name is not its shorter one.
        assert_eq!(l.locate_entry("Commander: not legal"), None);
    }

    #[test]
    fn a_near_miss_names_only_an_exactly_named_format() {
        let l = nadu();
        assert_eq!(
            l.near_entry("Modern: legal").as_deref(),
            Some("Modern: banned")
        );
        assert_eq!(l.near_entry("modern").as_deref(), Some("Modern: banned"));
        assert_eq!(
            l.near_entry("paupercommander: legal").as_deref(),
            Some("Pauper Commander: not legal")
        );
        // `Commander` is not `Pauper Commander`, in either direction.
        assert_eq!(
            l.near_entry("Commander: legal").as_deref(),
            Some("Commander: banned")
        );
        // A format the card has no entry for, a looser mention, a name that
        // holds two formats' names, and two entries at once name nothing.
        for loose in [
            "Legacy: banned",
            "banned in Modern",
            "Standard Pauper: legal",
            "Modern: banned · Vintage: legal",
        ] {
            assert_eq!(l.near_entry(loose), None, "{loose}");
        }
    }

    #[test]
    fn the_label_table_names_each_format_once() {
        for (i, (key, name)) in LABELS.iter().enumerate() {
            for (k, n) in LABELS.iter().skip(i + 1) {
                assert!(k != key && !n.eq_ignore_ascii_case(name), "{key} / {k}");
            }
        }
        assert_eq!(label("duel"), "Duel Commander");
        assert_eq!(label("tlr"), "tlr");
    }
}
