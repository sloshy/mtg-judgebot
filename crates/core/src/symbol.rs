//! Magic's card symbols (`{W}`, `{2/U}`, `{T}`) and the one rule for naming
//! them.
//!
//! Scryfall writes a symbol as a brace-wrapped *body* (`W`, `W/U`, `2/W`, `½`).
//! Discord renders one as a custom emoji, which is addressed by a name it
//! allows: `[A-Za-z0-9_]`, 2–32 characters. [`emoji_name`] is the mapping
//! between the two, and it lives here — pure, no I/O — because two separate
//! programs must agree on it exactly: `judge-ingest emoji` uploads the emoji
//! under these names, and the Discord adapter looks them up by the same names
//! at startup. A copy in each would be a silent drift waiting to happen.

/// Prefix of every emoji name that stands for a card symbol. Names outside it
/// are ignored when reading emoji back, so an unrelated emoji on the same
/// Discord application can never be substituted into a message.
pub const NAME_PREFIX: &str = "mana_";

/// Discord's limit on an emoji name, in characters.
pub const NAME_LIMIT: usize = 32;

/// Longest symbol body treated as a candidate. The longest Scryfall writes is
/// `1000000` at seven; the slack covers anything they add without letting a
/// stray `{` scan off to a distant `}`.
pub const MAX_BODY_CHARS: usize = 12;

/// The emoji name for a symbol body (the text between the braces): lowercased,
/// slashes dropped, `½`/`∞` spelled out, [`NAME_PREFIX`] in front.
/// `W/U` → `mana_wu`.
///
/// `None` if the body could not be a Magic symbol — anything but ASCII
/// alphanumerics, `/`, `½` and `∞`, or a name outside Discord's 2–32 range.
/// The `every_scryfall_symbol_maps_to_a_distinct_name` test pins this as total
/// and injective over every symbol Scryfall publishes.
#[must_use]
pub fn emoji_name(body: &str) -> Option<String> {
    if body.chars().count() > MAX_BODY_CHARS {
        return None;
    }
    let mut out = String::from(NAME_PREFIX);
    for ch in body.chars() {
        match ch {
            '/' => {}
            '½' => out.push_str("half"),
            '∞' => out.push_str("inf"),
            c if c.is_ascii_alphanumeric() => out.push(c.to_ascii_lowercase()),
            _ => return None,
        }
    }
    // A body of only slashes would leave the bare prefix.
    if out.len() <= NAME_PREFIX.len() || out.chars().count() > NAME_LIMIT {
        return None;
    }
    Some(out)
}

/// Whether `name` is one [`emoji_name`] could have produced, so a hand-made
/// emoji cannot smuggle unexpected characters into a substituted tag. Exact:
/// the prefix followed by one or more lowercase ASCII alphanumerics, nothing
/// else.
#[must_use]
pub fn is_emoji_name(name: &str) -> bool {
    let Some(body) = name.strip_prefix(NAME_PREFIX) else {
        return false;
    };
    !body.is_empty()
        && name.chars().count() <= NAME_LIMIT
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// `W/U` → `U/W`, for looking up a hybrid the writer put in the other order.
/// Only when both halves are single colour letters: `2/W` and `W/P` have a
/// fixed order and must never be flipped.
#[must_use]
pub fn flipped(body: &str) -> Option<String> {
    let (a, b) = body.split_once('/')?;
    let colour = |s: &str| {
        let mut cs = s.chars();
        matches!(
            (cs.next().map(|c| c.to_ascii_uppercase()), cs.next()),
            (Some('W' | 'U' | 'B' | 'R' | 'G' | 'C'), None)
        )
    };
    (colour(a) && colour(b)).then(|| format!("{b}/{a}"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Every symbol body `GET https://api.scryfall.com/symbology` returned on
    /// 2026-08-31, in order. The uploader reads the live endpoint; this is the
    /// fixture that keeps [`emoji_name`]'s totality claim honest.
    const SCRYFALL_BODIES: &[&str] = &[
        "T", "Q", "E", "P", "PW", "CHAOS", "A", "TK", "X", "Y", "Z", "0", "½", "1", "2", "3", "4",
        "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16", "17", "18", "19", "20",
        "100", "1000000", "∞", "W/U", "W/B", "B/R", "B/G", "U/B", "U/R", "R/G", "R/W", "G/W",
        "G/U", "B/G/P", "B/R/P", "G/U/P", "G/W/P", "R/G/P", "R/W/P", "U/B/P", "U/R/P", "W/B/P",
        "W/U/P", "C/W", "C/U", "C/B", "C/R", "C/G", "2/W", "2/U", "2/B", "2/R", "2/G", "H", "W/P",
        "U/P", "B/P", "R/P", "G/P", "C/P", "HW", "HR", "W", "U", "B", "R", "G", "C", "S", "L", "D",
    ];

    #[test]
    fn every_scryfall_symbol_maps_to_a_distinct_name() {
        assert_eq!(SCRYFALL_BODIES.len(), 84, "the fixture is the published set");
        let names: Vec<String> = SCRYFALL_BODIES.iter().filter_map(|b| emoji_name(b)).collect();
        // Total: every symbol names. Injective: no two share a name, so the
        // uploader cannot overwrite one symbol's emoji with another's.
        assert_eq!(
            names.len(),
            SCRYFALL_BODIES.len(),
            "a published symbol has no emoji name"
        );
        let distinct: HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "two symbols share a name");
        // And every name is one we would accept back off the wire.
        assert!(names.iter().all(|n| is_emoji_name(n)), "{names:?}");
    }

    #[test]
    fn names_are_lowercase_alphanumeric() {
        assert_eq!(emoji_name("W").as_deref(), Some("mana_w"));
        assert_eq!(emoji_name("W/U").as_deref(), Some("mana_wu"));
        assert_eq!(emoji_name("W/U/P").as_deref(), Some("mana_wup"));
        assert_eq!(emoji_name("2/W").as_deref(), Some("mana_2w"));
        assert_eq!(emoji_name("½").as_deref(), Some("mana_half"));
        assert_eq!(emoji_name("∞").as_deref(), Some("mana_inf"));
        // Not symbols.
        assert_eq!(emoji_name(""), None);
        assert_eq!(emoji_name("/"), None);
        assert_eq!(emoji_name("a b"), None);
        assert_eq!(emoji_name("emoji😀"), None);
        assert_eq!(emoji_name(&"W".repeat(MAX_BODY_CHARS + 1)), None);
        // Multi-byte bodies are measured in characters, not bytes.
        assert_eq!(emoji_name(&"∞".repeat(12)), None, "36 chars is over the limit");
    }

    #[test]
    fn is_emoji_name_is_the_exact_inverse_alphabet() {
        assert!(is_emoji_name("mana_w") && is_emoji_name("mana_1000000"));
        assert!(!is_emoji_name("mana_W"));
        assert!(!is_emoji_name("party_parrot"));
        assert!(!is_emoji_name("mana_"));
        // `emoji_name` never emits an underscore past the prefix.
        assert!(!is_emoji_name("mana__"));
        assert!(!is_emoji_name("mana_w_x"));
        assert!(!is_emoji_name(&format!("mana_{}", "w".repeat(NAME_LIMIT))));
    }

    #[test]
    fn only_two_colour_hybrids_flip() {
        assert_eq!(flipped("W/U").as_deref(), Some("U/W"));
        assert_eq!(flipped("C/W").as_deref(), Some("W/C"));
        assert_eq!(flipped("2/W"), None);
        assert_eq!(flipped("W/P"), None);
        assert_eq!(flipped("W/U/P"), None);
        assert_eq!(flipped("W"), None);
    }
}
