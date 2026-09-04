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
//! still a rejection.

use core::ops::Range;

/// The canonical form of `c` for quote comparison.
///
/// Quotation marks and apostrophes fold to `'` and `"`, the dash block and
/// the minus sign to `-`, and the non-breaking/typographic spaces (plus tab)
/// to a plain space. Line breaks are deliberately *not* folded: a quote may
/// not span lines, and a stored quote stays a single line.
#[must_use]
pub fn canonical(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '\u{00B4}' | '\u{0060}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{2043}' => '-',
        '\t' | '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

/// Byte range in `source` of the first span whose canonical form equals
/// `quote`'s, if any. An empty `quote` names no span.
#[must_use]
pub fn find(source: &str, quote: &str) -> Option<Range<usize>> {
    let q: Vec<char> = quote.chars().map(canonical).collect();
    if q.is_empty() {
        return None;
    }
    let s: Vec<(usize, char)> = source.char_indices().map(|(i, c)| (i, canonical(c))).collect();
    let last = s.len().checked_sub(q.len())?;
    (0..=last).find_map(|w| {
        let matches = s.iter().skip(w).zip(&q).all(|((_, a), b)| a == b);
        if !matches {
            return None;
        }
        let start = s.get(w)?.0;
        let end = s.get(w + q.len()).map_or(source.len(), |&(i, _)| i);
        Some(start..end)
    })
}

/// The source's own text for the span `quote` names, if it names one.
///
/// This is the value a citation should carry: `locate(src, q) == Some(q)`
/// whenever the model copied the text exactly, and the typeset original
/// whenever it retyped the punctuation.
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
