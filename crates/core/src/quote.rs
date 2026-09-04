//! Verbatim quote matching that tolerates typographic punctuation.
//!
//! Every citation carries a `quote` that must be the *source's own text*. The
//! CR, Scryfall's rulings and Oracle text are typeset with curly apostrophes
//! (`doesn’t`), curly quotation marks (`“[Cost]: [Effect].”`), em dashes and
//! non-breaking spaces; models reliably retype a handful of those as their
//! ASCII cousins even when the prompt forbids it. A byte-exact `contains`
//! then rejects a citation that is *right about the rule and the span* over
//! one character of punctuation, spends the pipeline's single retry, and can
//! push the model into inventing a placeholder rather than trying again.
//!
//! So matching compares a canonical form of each `char` — and returns the
//! **source's** slice, never the model's. Snapping the quote back to the text
//! it names is what keeps the leniency from leaking: a stored quote is by
//! construction a byte-exact substring of its source, so
//! [`citation_supported`](crate::citation_supported) (which the retirement
//! pass re-runs against tomorrow's rules) stays a strict check, and Discord
//! renders the CR's own typography.
//!
//! The mapping is one `char` to one `char`, so a canonical position is also a
//! source position; nothing here collapses or inserts characters. Case,
//! word order and line breaks are still matched exactly: a paraphrase is
//! still a rejection. The search itself stays linear (`str::find` over the
//! canonical text, then the index mapped back), because a citation's quote is
//! not length-bounded anywhere: an outside caller of the session API could
//! otherwise hand `submit_verdict` many long quotes against a long rule and
//! pay for a window scan of each.

use core::ops::Range;

/// The canonical form of `c` for quote comparison.
///
/// Quotation marks and apostrophes fold to `'` and `"`; the dash block, the
/// minus sign, the bullets and `*` to `-`; the non-breaking and typographic
/// spaces (plus tab) to a plain space. Line breaks are deliberately *not*
/// folded: a quote may not span lines, and a stored quote stays a single
/// line. Nor is the ellipsis: `…` is one `char` and `...` is three, and the
/// one-to-one mapping is what makes a canonical offset a source offset.
/// Letters are left alone — an accent is a spelling, not typography.
#[must_use]
pub fn canonical(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '\u{00B4}' | '\u{0060}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        // The dash block and the minus sign, and with them the bullets: modal
        // spells ("Choose one —") mark their modes with U+2022, 2000-odd
        // characters of Oracle text that a model may retype as `-` or `*`.
        // Folding the ASCII stand-ins into the same class is what makes that
        // work, and letting the bullets share it with the dashes costs
        // nothing, because the span that comes back is the source's own text
        // either way.
        '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{2043}' | '\u{2022}' | '\u{2023}' | '*' => '-',
        '\t' | '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

/// Byte range in `source` of the first span whose canonical form equals
/// `quote`'s, if any. An empty `quote` names no span.
///
/// Linear in both lengths: the search runs over the canonical text with
/// `str::find`, and the canonical byte offset it reports is mapped back
/// through a table built in the same pass.
#[must_use]
pub fn find(source: &str, quote: &str) -> Option<Range<usize>> {
    if quote.is_empty() {
        return None;
    }
    let needle: String = quote.chars().map(canonical).collect();
    // `canonical` maps one `char` to one `char`, but not to one of the same
    // width, so the canonical text needs its own offsets: `origin[i]` is the
    // source byte offset of the character whose canonical form starts at
    // canonical byte `i`, with a final entry for the end of the text.
    let mut haystack = String::with_capacity(source.len());
    let mut origin = Vec::with_capacity(source.len() + 1);
    for (at, c) in source.char_indices() {
        let folded = canonical(c);
        haystack.push(folded);
        origin.extend(core::iter::repeat_n(at, folded.len_utf8()));
    }
    origin.push(source.len());
    let start = haystack.find(&needle)?;
    // Both ends land on canonical char boundaries, so both are in the table.
    Some(*origin.get(start)?..*origin.get(start + needle.len())?)
}

/// The source's own text for the span `quote` names, if it names one.
///
/// This is the value a citation should carry: the typeset original of what
/// the model wrote. It is the *first* canonically equal span, not the closest
/// one — so where a source says `you can’t choose` in one sentence and `you
/// can't choose` in another, an exact copy of the second comes back as the
/// first. Both spans read the same word for word, and a citation stores a
/// string rather than an offset, so this costs nothing but is worth knowing
/// before reading a stored quote as "the span the model pointed at".
#[must_use]
pub fn locate<'a>(source: &'a str, quote: &str) -> Option<&'a str> {
    find(source, quote).and_then(|r| source.get(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULING: &str = "Havengul Lich’s ability changes the zone you are permitted to cast the card from, not the times you are permitted to cast it.";

    /// The failure this module exists for: the model retypes `’` as `'`.
    #[test]
    fn an_ascii_apostrophe_finds_the_curly_original() {
        let typed = "Havengul Lich's ability changes the zone";
        assert_eq!(locate(RULING, typed), Some("Havengul Lich’s ability changes the zone"));
    }

    /// An exact copy is returned unchanged, byte for byte.
    #[test]
    fn an_exact_copy_is_itself() {
        assert_eq!(locate(RULING, RULING), Some(RULING));
        let tail = "not the times you are permitted to cast it.";
        assert_eq!(locate(RULING, tail), Some(tail));
    }

    /// Quotation marks, dashes and non-breaking spaces all fold, and what
    /// comes back is the source's typography, not the model's.
    #[test]
    fn quotation_marks_dashes_and_spaces_fold() {
        let body = "They are written as “[Cost]: [Effect.]” — see rule 602.";
        assert_eq!(
            locate(body, r#"written as "[Cost]: [Effect.]" - see rule 602."#),
            Some("written as “[Cost]: [Effect.]” — see rule 602.")
        );
        let nbsp = "a mana\u{00A0}ability";
        assert_eq!(locate(nbsp, "a mana ability"), Some(nbsp));
        // Modal spells: the bullet is the most common non-ASCII character in
        // Oracle text after the apostrophe.
        let modal = "Choose one —\n• Target creature gets +2/+0.";
        assert_eq!(locate(modal, "- Target creature gets +2/+0."), Some("• Target creature gets +2/+0."));
        assert_eq!(locate(modal, "* Target creature gets +2/+0."), Some("• Target creature gets +2/+0."));
        // The ellipsis is not folded: it cannot be, one char to one char.
        assert_eq!(locate("wait…now", "wait...now"), None);
    }

    /// Leniency stops at punctuation: a paraphrase, a dropped word, a case
    /// change or a quote spanning a line break is still not a quote.
    #[test]
    fn only_punctuation_is_forgiven() {
        assert_eq!(locate(RULING, "Havengul Lich changes the zone"), None);
        assert_eq!(locate(RULING, "havengul lich’s ability"), None);
        assert_eq!(locate("one line\nnext line", "one line next line"), None);
        assert_eq!(locate(RULING, ""), None);
        assert_eq!(locate("", "anything"), None);
    }

    /// The first canonically equal span wins, even when a later one is a
    /// byte-exact copy of the quote. Harmless — they read the same — but it
    /// means a stored quote is not a pointer to a position.
    #[test]
    fn the_first_canonical_match_wins() {
        let src = "you can’t choose that, and you can't choose this";
        assert_eq!(locate(src, "you can't choose"), Some("you can’t choose"));
    }

    /// Byte offsets: folding changes the byte length of a span, so the range
    /// must be built from the source's own indices. Slicing it must never
    /// panic on a char boundary.
    #[test]
    fn the_range_is_a_source_byte_range() {
        let src = "x’y";
        assert_eq!(find(src, "’y"), Some(1..src.len()));
        assert_eq!(find(src, "’y").and_then(|r| src.get(r)), Some("’y"));
        assert_eq!(locate(src, "'y"), Some("’y"));
    }
}
