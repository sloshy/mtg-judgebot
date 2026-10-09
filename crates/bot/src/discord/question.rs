//! Rewriting a question after a "did you mean…?" pick: the ambiguous span is
//! replaced by `[[Full Card Name]]`, which the resolver's bracket step then
//! matches exactly (ARCHITECTURE.md §3 step 2).
//!
//! The Discord pick is two halves of the same rewrite: [`find_span`] runs
//! when the buttons are offered and its range travels in the button's
//! `custom_id`; [`pin_at`] runs on the click, on the question read back from
//! the message. [`pin_card`] is the two in one, and a test holds them equal.

use std::ops::Range;

/// Replace the first occurrence of `span` in `text` with `[[name]]`.
///
/// The extractor emits spans verbatim, so an exact match is the normal case;
/// a case-insensitive match is the fallback, and among matches one at word
/// boundaries wins ("bolt" in "bolt kill Boltwing" picks the first word, not
/// the prefix of the second). If the span is not in the text at all, the
/// pinned name is appended in parentheses so the next run still sees it.
#[must_use]
pub fn pin_card(text: &str, span: &str, name: &str) -> String {
    let at = find_span(text, span);
    // `find_span` returns a range of `text` on char boundaries, so this is
    // never `None`; the append is the same answer a missing span gets.
    pin_at(text, at, name).unwrap_or_else(|| append(text, name))
}

/// Replace the byte range `at` of `text` with `[[name]]`, or with no range
/// append ` ([[name]])` as [`pin_card`] does for a span it cannot find.
/// `None` when `at` is empty, out of bounds or not on char boundaries: the
/// range came from outside (a button), so it is checked, never trusted.
#[must_use]
pub fn pin_at(text: &str, at: Option<Range<usize>>, name: &str) -> Option<String> {
    let Some(r) = at else {
        return Some(append(text, name));
    };
    if r.start >= r.end {
        return None;
    }
    let before = text.get(..r.start)?;
    // Checks that the range itself lies on char boundaries.
    text.get(r.clone())?;
    let after = text.get(r.end..)?;
    Some(format!("{before}{}{after}", pinned(name)))
}

fn pinned(name: &str) -> String {
    format!("[[{}]]", name.trim())
}

fn append(text: &str, name: &str) -> String {
    format!("{} ({})", text.trim_end(), pinned(name))
}

/// Byte range of the occurrence of `span` (trimmed) in `text` that a pick
/// replaces, or `None` when it is not there (the pick then appends).
#[must_use]
pub fn find_span(text: &str, span: &str) -> Option<Range<usize>> {
    let span = span.trim();
    if span.is_empty() {
        return None;
    }
    if let Some(i) = text.find(span) {
        return Some(i..i + span.len());
    }
    let all: Vec<Range<usize>> = text
        .char_indices()
        .filter_map(|(start, _)| match_at(text, start, span))
        .collect();
    let bounded = |r: &Range<usize>| {
        let before = text.get(..r.start).and_then(|s| s.chars().next_back());
        let after = text.get(r.end..).and_then(|s| s.chars().next());
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    };
    all.iter()
        .find(|r| bounded(r))
        .or_else(|| all.first())
        .cloned()
}

/// Case-insensitive match of `span` at byte offset `start` of `text`.
fn match_at(text: &str, start: usize, span: &str) -> Option<Range<usize>> {
    let mut hay = text.get(start..)?.char_indices();
    let mut end = start;
    for want in span.chars() {
        let (off, ch) = hay.next()?;
        if !ch.to_lowercase().eq(want.to_lowercase()) {
            return None;
        }
        end = start + off + ch.len_utf8();
    }
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_span_is_replaced_in_place() {
        assert_eq!(
            pin_card("Does urza tap for two?", "urza", "Urza's Tower"),
            "Does [[Urza's Tower]] tap for two?"
        );
        // Only the first occurrence.
        assert_eq!(
            pin_card("bob and bob", "bob", "Dark Confidant"),
            "[[Dark Confidant]] and bob"
        );
        // A bracketed span is replaced wholesale, brackets included.
        assert_eq!(
            pin_card("play [[Bruna]] now", "[[Bruna]]", "Bruna, the Fading Light"),
            "play [[Bruna, the Fading Light]] now"
        );
    }

    #[test]
    fn falls_back_to_case_insensitive_and_prefers_word_boundaries() {
        assert_eq!(
            pin_card("Does URZA tap?", "urza", "Urza's Mine"),
            "Does [[Urza's Mine]] tap?"
        );
        assert_eq!(
            pin_card("Can Boltwing die to Bolt?", "bolt", "Lightning Bolt"),
            "Can Boltwing die to [[Lightning Bolt]]?"
        );
        // No bounded match at all: the first case-insensitive one is used.
        assert_eq!(
            pin_card("Boltwing?", "bolt", "Lightning Bolt"),
            "[[Lightning Bolt]]wing?"
        );
        // Non-ASCII, multi-byte text on both sides.
        assert_eq!(
            pin_card("¿Qué hace lim-dûl?", "Lim-Dûl", "Lim-Dûl the Necromancer"),
            "¿Qué hace [[Lim-Dûl the Necromancer]]?"
        );
    }

    #[test]
    fn missing_span_is_appended() {
        assert_eq!(
            pin_card("what happens?  ", "jace", "Jace Beleren"),
            "what happens? ([[Jace Beleren]])"
        );
        assert_eq!(
            pin_card("what happens?", "   ", "Jace Beleren"),
            "what happens? ([[Jace Beleren]])"
        );
        assert_eq!(pin_card("", "x", "Jace Beleren"), " ([[Jace Beleren]])");
    }

    #[test]
    fn pin_at_agrees_with_pin_card() {
        let cases = [
            ("Does urza tap for two?", "urza"),
            ("bob and bob", "bob"),
            ("play [[Bruna]] now", "[[Bruna]]"),
            ("Does URZA tap?", "urza"),
            ("Can Boltwing die to Bolt?", "bolt"),
            ("Boltwing?", "bolt"),
            ("¿Qué hace lim-dûl?", "Lim-Dûl"),
            ("what happens?  ", "jace"),
            ("what happens?", "   "),
            ("", "x"),
            ("is bob good", " bob "),
            ("emoji 🃏 then bob\n\nand more", "BOB"),
        ];
        for (text, span) in cases {
            let at = find_span(text, span);
            assert_eq!(
                pin_at(text, at, " Card Name "),
                Some(pin_card(text, span, " Card Name ")),
                "{text:?} / {span:?}"
            );
        }
    }

    #[test]
    fn pin_at_refuses_a_range_that_is_not_in_the_text() {
        let text = "¿Qué hace lim-dûl?";
        // Empty, reversed, past the end, and inside the two-byte `¿`.
        for (start, end) in [(3, 3), (5, 2), (10, 40), (1, 4)] {
            assert_eq!(pin_at(text, Some(start..end), "X"), None, "{start}..{end}");
        }
        assert_eq!(
            pin_at(text, Some(12..20), "X").as_deref(),
            Some("¿Qué hace [[X]]?")
        );
    }

    #[test]
    fn span_and_name_are_trimmed() {
        assert_eq!(
            pin_card("is bob good", " bob ", "  Dark Confidant "),
            "is [[Dark Confidant]] good"
        );
    }
}
