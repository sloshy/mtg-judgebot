//! Magic's card symbols (`{W}`, `{2/U}`, `{T}`) as Discord custom emoji.
//!
//! Discord has no mana symbols, so the bot uploads one *application* emoji per
//! Scryfall symbol (see the `judge-ingest emoji` subcommand) and substitutes
//! `<:mana_w:123…>` into the text it sends. Application emoji belong to the bot
//! rather than to a server, so they work in every guild it posts in.
//!
//! Two things make this more than a string replace:
//!
//! * **A tag is atomic.** `{T}` is three characters; `<:mana_t:123…>` is about
//!   thirty, and Discord counts the tag against the 2000/4096 limits. Cutting
//!   text to fit must therefore drop a whole tag or none of it — a half-written
//!   `<:mana_t:12` is visible garbage. [`Rendered`] makes that unrepresentable:
//!   text is a list of segments, and only [`Segment::Text`] can be cut.
//! * **The table can be empty.** A deployment that has not uploaded the emoji
//!   yet gets [`SymbolTable::empty`], and every symbol stays the literal `{W}`
//!   Scryfall writes. That is the pre-emoji behaviour, so nothing breaks.

use std::{collections::HashMap, fmt};

use judge_core::symbol::{self, MAX_BODY_CHARS};

use super::render::TRUNCATION_MARKER;

/// The card-symbol emoji this bot's application owns, ready to substitute.
///
/// Built from Discord's answer at startup rather than from a hard-coded list,
/// so uploading the symbols Scryfall adds later needs no code change here.
#[derive(Clone, Debug, Default)]
pub struct SymbolTable {
    /// Emoji name → the finished `<:name:id>` tag.
    tags: HashMap<String, String>,
}

impl SymbolTable {
    /// No symbols: every `{W}` is left exactly as written.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from the application's emoji as `(name, id)` pairs. Names that
    /// are not [`symbol::is_emoji_name`] are dropped.
    #[must_use]
    pub fn new(emojis: impl IntoIterator<Item = (String, u64)>) -> Self {
        let tags = emojis
            .into_iter()
            .filter(|(name, _)| symbol::is_emoji_name(name))
            .map(|(name, id)| {
                let tag = format!("<:{name}:{id}>");
                (name, tag)
            })
            .collect();
        Self { tags }
    }

    /// How many symbols are available.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    /// Whether no symbol will be substituted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    /// The tag for a symbol body, trying the flipped hybrid as a fallback.
    #[must_use]
    fn tag(&self, body: &str) -> Option<&str> {
        let direct = symbol::emoji_name(body).and_then(|n| self.tags.get(&n));
        direct
            .or_else(|| {
                symbol::flipped(body)
                    .and_then(|f| symbol::emoji_name(&f))
                    .and_then(|n| self.tags.get(&n))
            })
            .map(String::as_str)
    }
}

/// One stretch of a [`Rendered`] message.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    /// Plain text. May be cut at any character boundary.
    Text(String),
    /// One `<:mana_w:123…>` tag. Kept whole or dropped whole.
    Emoji(String),
}

impl Segment {
    /// The characters Discord counts for this segment.
    fn len(&self) -> usize {
        match self {
            Self::Text(s) | Self::Emoji(s) => s.chars().count(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Text(s) | Self::Emoji(s) => s.is_empty(),
        }
    }
}

/// Text with the card symbols already turned into emoji tags, still knowing
/// where those tags are so truncation cannot split one.
///
/// [`Display`](fmt::Display) gives the untruncated string; [`Rendered::fit`] is
/// what actually goes to Discord.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered(Vec<Segment>);

impl Rendered {
    /// Text known to hold no symbols (a URL, a markdown label, a card name).
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            return Self::default();
        }
        Self(vec![Segment::Text(text)])
    }

    /// Substitute every symbol in `text` that `table` has an emoji for. A
    /// symbol it does not have (and anything else in braces) is left verbatim.
    #[must_use]
    pub fn substitute(text: &str, table: &SymbolTable) -> Self {
        let mut segments: Vec<Segment> = Vec::new();
        let mut plain = String::new();
        let mut rest = text;
        loop {
            let Some(open) = rest.find('{') else {
                plain.push_str(rest);
                break;
            };
            let (before, at_brace) = rest.split_at(open);
            plain.push_str(before);
            // Past the '{', which is one byte, so this is a char boundary.
            let after = at_brace.get(1..).unwrap_or_default();
            // Only the next MAX_BODY_CHARS characters can be a symbol body, so
            // a stray `{` can neither pair with a distant `}` nor make the scan
            // quadratic on text full of braces.
            let close = after
                .char_indices()
                .take(MAX_BODY_CHARS.saturating_add(1))
                .find_map(|(i, c)| (c == '}').then_some(i));
            let hit = close.and_then(|end| {
                let tag = table.tag(after.get(..end)?)?.to_owned();
                let tail = after.get(end.saturating_add(1)..).unwrap_or_default();
                Some((tag, tail))
            });
            if let Some((tag, tail)) = hit {
                if !plain.is_empty() {
                    segments.push(Segment::Text(std::mem::take(&mut plain)));
                }
                segments.push(Segment::Emoji(tag));
                rest = tail;
            } else {
                // Not a symbol we have: keep the brace and rescan from after
                // it, so `{{W}` still finds the `{W}`.
                plain.push('{');
                rest = after;
            }
        }
        if !plain.is_empty() {
            segments.push(Segment::Text(plain));
        }
        Self(segments)
    }

    /// Append plain text.
    pub fn push_str(&mut self, text: &str) {
        if !text.is_empty() {
            self.0.push(Segment::Text(text.to_owned()));
        }
    }

    /// Append another rendered stretch.
    pub fn append(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// The characters Discord counts for the whole thing.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.iter().fold(0usize, |n, s| n.saturating_add(s.len()))
    }

    /// Whether there is nothing to send. Cheap: it does not count characters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Segment::is_empty)
    }

    /// Cut to at most `limit` characters, with [`TRUNCATION_MARKER`] if
    /// anything was dropped, still as a [`Rendered`] so the result can be
    /// bounded again or joined with others. An emoji tag is never split: it is
    /// kept whole or dropped whole, and everything after a tag that did not fit
    /// goes with it.
    #[must_use]
    pub fn truncate(&self, limit: usize) -> Self {
        let mut whole = self.clone();
        whole.trim_end();
        if whole.len() <= limit {
            return whole;
        }
        let marker = TRUNCATION_MARKER.chars().count();
        if limit < marker {
            return self.take(limit);
        }
        let mut out = self.take(limit.saturating_sub(marker));
        out.trim_end();
        out.push_str(TRUNCATION_MARKER);
        out
    }

    /// [`Self::truncate`] as the string Discord is sent. Matches
    /// [`render::fit`](super::render::fit) on text with no symbols in it.
    #[must_use]
    pub fn fit(&self, limit: usize) -> String {
        self.truncate(limit).to_string()
    }

    /// Drop trailing whitespace, as [`str::trim_end`] does. A tag never ends in
    /// whitespace, so only trailing text is ever touched.
    fn trim_end(&mut self) {
        while let Some(Segment::Text(text)) = self.0.last_mut() {
            let kept = text.trim_end().len();
            text.truncate(kept);
            if text.is_empty() {
                self.0.pop();
            } else {
                break;
            }
        }
    }

    /// The first `budget` characters, stopping before a tag that would not fit.
    fn take(&self, budget: usize) -> Self {
        let mut out: Vec<Segment> = Vec::new();
        let mut used = 0usize;
        for segment in &self.0 {
            let room = budget.saturating_sub(used);
            if room == 0 {
                break;
            }
            match segment {
                Segment::Text(text) => {
                    let n = text.chars().count();
                    if n > room {
                        out.push(Segment::Text(text.chars().take(room).collect()));
                        break;
                    }
                    out.push(Segment::Text(text.clone()));
                    used = used.saturating_add(n);
                }
                Segment::Emoji(tag) => {
                    let n = tag.chars().count();
                    if n > room {
                        break;
                    }
                    out.push(Segment::Emoji(tag.clone()));
                    used = used.saturating_add(n);
                }
            }
        }
        Self(out)
    }
}

impl fmt::Display for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for segment in &self.0 {
            match segment {
                Segment::Text(s) | Segment::Emoji(s) => f.write_str(s)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are 17–20 digits in practice; these are the right shape.
    fn table(symbols: &[&str]) -> SymbolTable {
        SymbolTable::new(symbols.iter().enumerate().filter_map(|(i, body)| {
            let id = 1_000_000_000_000_000_000u64.saturating_add(i as u64);
            Some((symbol::emoji_name(body)?, id))
        }))
    }

    fn full() -> SymbolTable {
        table(&[
            "W", "U", "B", "R", "G", "C", "T", "Q", "X", "1", "2", "10", "W/U", "2/W", "W/P",
            "C/W", "½", "∞", "1000000",
        ])
    }

    /// A brace that opens nothing must not make the scan walk the rest of the
    /// text looking for a partner.
    #[test]
    fn many_open_braces_stay_linear() {
        let t = full();
        for n in [1_000usize, 20_000] {
            let text = format!("{}{{W}}", "{".repeat(n));
            let started = std::time::Instant::now();
            let out = Rendered::substitute(&text, &t).to_string();
            assert!(out.starts_with('{') && out.ends_with('>'), "n={n}");
            assert!(
                started.elapsed() < std::time::Duration::from_millis(200),
                "n={n} took {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn substitution_replaces_known_symbols_and_leaves_the_rest() {
        let t = full();
        let out = Rendered::substitute("{T}: Add {W} or {G}.", &t).to_string();
        assert_eq!(
            out,
            "<:mana_t:1000000000000000006>: Add <:mana_w:1000000000000000000> or <:mana_g:1000000000000000004>."
        );
        // Unknown symbol, unknown brace content and a lone brace survive.
        let out = Rendered::substitute("{ZZZ} {not a symbol} { {W}", &t).to_string();
        assert_eq!(out, "{ZZZ} {not a symbol} { <:mana_w:1000000000000000000>");
        // A doubled brace still finds the symbol inside it.
        assert!(
            Rendered::substitute("{{W}", &t)
                .to_string()
                .starts_with("{<:mana_w:")
        );
        // An unclosed brace is not a runaway scan.
        assert_eq!(
            Rendered::substitute("{W is not closed", &t).to_string(),
            "{W is not closed"
        );
        // Lowercase and flipped hybrids resolve to the canonical emoji.
        let w = Rendered::substitute("{W}", &t).to_string();
        assert_eq!(Rendered::substitute("{w}", &t).to_string(), w);
        let wu = Rendered::substitute("{W/U}", &t).to_string();
        assert_eq!(Rendered::substitute("{U/W}", &t).to_string(), wu);
        // Ordered symbols are never flipped: {W/2} is not {2/W}.
        assert_eq!(Rendered::substitute("{W/2}", &t).to_string(), "{W/2}");
        assert_eq!(Rendered::substitute("{P/W}", &t).to_string(), "{P/W}");
    }

    #[test]
    fn an_empty_table_is_the_pre_emoji_behaviour() {
        let text = "{T}: Add {W}. {½} {∞}";
        let r = Rendered::substitute(text, &SymbolTable::empty());
        assert_eq!(r.to_string(), text);
        assert_eq!(r.len(), text.chars().count());
        assert_eq!(r.fit(1000), text);
        assert!(SymbolTable::empty().is_empty());
    }

    #[test]
    fn only_mana_prefixed_emoji_enter_the_table() {
        let t = SymbolTable::new([
            ("mana_w".to_owned(), 1),
            ("party_parrot".to_owned(), 2),
            ("mana_W".to_owned(), 3),
        ]);
        assert_eq!(t.len(), 1);
        assert_eq!(Rendered::substitute("{W}", &t).to_string(), "<:mana_w:1>");
        // The fixture table really does hold every symbol it was built from.
        assert_eq!(full().len(), 19);
    }

    #[test]
    fn fit_never_splits_a_tag() {
        let t = full();
        let r = Rendered::substitute("{W}{U}{B}", &t);
        let tag = "<:mana_w:1000000000000000000>".chars().count();
        assert_eq!(r.len(), tag * 3);
        // Enough room for two tags and the marker, not the third.
        let out = r.fit(tag * 2 + TRUNCATION_MARKER.chars().count());
        assert_eq!(out.matches("<:mana_").count(), 2);
        assert!(out.ends_with(TRUNCATION_MARKER));
        assert!(!out.contains("<:mana_b"));
        // Every budget produces a string that is whole tags plus plain text.
        for limit in 0..=r.len() {
            let out = r.fit(limit);
            assert!(out.chars().count() <= limit, "limit {limit}: {out}");
            assert_eq!(
                out.matches("<:").count(),
                out.matches('>').count(),
                "limit {limit} split a tag: {out}"
            );
        }
    }

    #[test]
    fn fit_matches_plain_text_truncation() {
        // With no symbols, Rendered::fit and render::fit agree everywhere.
        let text = "The quick brown fox jumps over the lazy dog.  ";
        let r = Rendered::plain(text);
        for limit in 0..text.chars().count() + 5 {
            assert_eq!(
                r.fit(limit),
                super::super::render::fit(text, limit),
                "limit {limit}"
            );
        }
    }

    #[test]
    fn text_after_a_dropped_tag_is_dropped_too() {
        let t = full();
        // A tag that does not fit ends the message, even though "ok" would.
        let mut r = Rendered::substitute("{W}", &t);
        r.push_str("ok");
        let out = r.fit(6);
        assert!(!out.contains("ok"), "{out}");
        assert!(out.chars().count() <= 6);
    }

    #[test]
    fn plain_and_append_compose() {
        let t = full();
        let mut r = Rendered::plain("[Oracle text] “");
        r.append(Rendered::substitute("{T}: Draw.", &t));
        r.push_str("”");
        assert!(r.to_string().starts_with("[Oracle text] “<:mana_t:"));
        assert!(r.to_string().ends_with(": Draw.”"));
        assert_eq!(r.len(), r.to_string().chars().count());
        assert!(Rendered::plain("").is_empty());
        assert!(Rendered::default().is_empty());
        assert_eq!(Rendered::default().fit(10), "");
    }
}
