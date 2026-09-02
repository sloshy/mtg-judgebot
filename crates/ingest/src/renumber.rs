//! Follow rules that a CR release renumbered, so the calls citing them move too.
//!
//! When Wizards inserts a keyword at 702.20, every later rule in the section
//! shifts by one. The rules are the same rules, but their ids changed — and so
//! did their bodies, because rule text is full of cross-references ("see rule
//! 702.21") that were renumbered along with them. Exact text comparison
//! therefore fails on precisely the release it is meant to see through.
//!
//! [`renumber_map`] compares bodies with every rule id *masked* out: two rules
//! are the same rule when their masked bodies are equal and that masked body
//! is unique on both sides. Anything ambiguous is left alone — the retirement
//! pass then decides on the citations' own merits — so a relocation is never a
//! guess. Leaves are matched under their (possibly relocated) parent, which
//! handles a lettered sub-rule inserted mid-list.
//!
//! [`rewrite_call`] applies a map to one stored call consistently: the ids of
//! its `rule` citations, the rule ids *inside* the quotes of its `rule` and
//! `prior_call` citations, and its answer text — all in one pass so a chain
//! like 702.19→702.20→702.21 never double-applies. Quotes of `scryfall_ruling`
//! and `oracle_text` citations are left alone: Scryfall's text was not
//! renumbered, and a rewritten quote would stop matching it. `prior_call`
//! quotes are rewritten because the answers they quote are rewritten by the
//! same map.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use judge_core::{RuleChunk, RuleId};
use regex::Regex;

/// A rule id as it appears in running text: `702.19`, `702.19b`, `704.5aa`.
/// Three-digit section numbers alone (`rule 704`) are not rows and not matched.
static RULE_REF: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"\b[0-9]{3}\.[0-9]+[a-z]{0,2}\b").expect("RULE_REF is a valid regex")
});

/// A rule row as stored before the new release is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRule {
    pub id: RuleId,
    pub parent_id: Option<RuleId>,
    pub body: String,
}

/// `body` with every rule reference replaced by `#`.
fn masked(body: &str) -> String {
    RULE_REF.replace_all(body, "#").into_owned()
}

/// Group by masked body, keeping only bodies that occur exactly once.
fn unique_by_masked<'a, I>(rules: I) -> HashMap<String, &'a RuleId>
where
    I: IntoIterator<Item = (&'a RuleId, &'a str)>,
{
    let mut counts: HashMap<String, (usize, &'a RuleId)> = HashMap::new();
    for (id, body) in rules {
        let e = counts.entry(masked(body)).or_insert((0, id));
        e.0 += 1;
    }
    counts.into_iter().filter(|(_, (n, _))| *n == 1).map(|(k, (_, id))| (k, id)).collect()
}

/// Old id → new id for every rule whose id changed but whose masked body is
/// unchanged and unique on both sides. Rules that kept their id are absent, as
/// are rules whose text changed (they cannot be told apart from new rules) and
/// rules whose masked body is shared with another rule (ambiguous).
#[must_use]
pub fn renumber_map(old: &[StoredRule], new: &[RuleChunk]) -> BTreeMap<RuleId, RuleId> {
    let mut map = BTreeMap::new();

    // Rule level: unique masked body on both sides.
    let old_rules = unique_by_masked(old.iter().filter(|r| r.parent_id.is_none()).map(|r| (&r.id, r.body.as_str())));
    let new_rules = unique_by_masked(new.iter().filter(|r| r.parent_id.is_none()).map(|r| (&r.id, r.body.as_str())));
    for (body, old_id) in &old_rules {
        if let Some(new_id) = new_rules.get(body)
            && *new_id != *old_id
        {
            map.insert((*old_id).clone(), (*new_id).clone());
        }
    }

    // Leaves: under the parent's new id (mapped, else unchanged), unique masked
    // body among that parent's leaves on both sides.
    let mut new_leaves_by_parent: HashMap<&RuleId, Vec<(&RuleId, &str)>> = HashMap::new();
    for r in new {
        if let Some(p) = &r.parent_id {
            new_leaves_by_parent.entry(p).or_default().push((&r.id, r.body.as_str()));
        }
    }
    let mut old_leaves_by_parent: HashMap<&RuleId, Vec<(&RuleId, &str)>> = HashMap::new();
    for r in old {
        if let Some(p) = &r.parent_id {
            old_leaves_by_parent.entry(p).or_default().push((&r.id, r.body.as_str()));
        }
    }
    for (old_parent, old_leaves) in &old_leaves_by_parent {
        let new_parent = map.get(*old_parent).unwrap_or(old_parent);
        let Some(new_leaves) = new_leaves_by_parent.get(new_parent) else { continue };
        let old_u = unique_by_masked(old_leaves.iter().copied());
        let new_u = unique_by_masked(new_leaves.iter().copied());
        for (body, old_id) in &old_u {
            if let Some(new_id) = new_u.get(body)
                && *new_id != *old_id
            {
                map.insert((*old_id).clone(), (*new_id).clone());
            }
        }
    }
    map
}

/// Replace every rule reference in `text` that has an entry in `map`, in one
/// pass (so a chain never double-applies). Returns `None` if nothing changed.
#[must_use]
pub fn rewrite_ids(text: &str, map: &BTreeMap<RuleId, RuleId>) -> Option<String> {
    let mut changed = false;
    let out = RULE_REF.replace_all(text, |caps: &regex::Captures<'_>| {
        let found = caps.get(0).map_or("", |m| m.as_str());
        match RuleId::try_new(found.to_owned()).ok().and_then(|id| map.get(&id)) {
            Some(to) => {
                changed = true;
                to.as_ref().to_owned()
            }
            None => found.to_owned(),
        }
    });
    changed.then(|| out.into_owned())
}

/// A stored call's `citations` array and `answer` after `map`. Works on the
/// JSON as stored so that an element which no longer decodes as a `Citation`
/// is carried through untouched rather than failing the whole call. Returns
/// `None` when nothing changed.
#[must_use]
pub fn rewrite_call(
    citations: &serde_json::Value,
    answer: &str,
    map: &BTreeMap<RuleId, RuleId>,
) -> Option<(serde_json::Value, String)> {
    let mut changed = false;
    let mut citations = citations.clone();
    if let Some(items) = citations.as_array_mut() {
        for c in items.iter_mut() {
            let Some(obj) = c.as_object_mut() else { continue };
            let kind = obj.get("kind").and_then(|k| k.as_str()).unwrap_or_default().to_owned();
            if kind == "rule"
                && let Some(id) = obj.get("id").and_then(|v| v.as_str())
                && let Some(to) = RuleId::try_new(id.to_owned()).ok().and_then(|id| map.get(&id))
            {
                obj.insert("id".to_owned(), serde_json::Value::String(to.as_ref().to_owned()));
                changed = true;
            }
            if (kind == "rule" || kind == "prior_call")
                && let Some(q) = obj.get("quote").and_then(|v| v.as_str())
                && let Some(new_q) = rewrite_ids(q, map)
            {
                obj.insert("quote".to_owned(), serde_json::Value::String(new_q));
                changed = true;
            }
        }
    }
    let answer = match rewrite_ids(answer, map) {
        Some(a) => {
            changed = true;
            a
        }
        None => answer.to_owned(),
    };
    changed.then_some((citations, answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_core::CrVersion;

    fn rid(s: &str) -> RuleId {
        #[allow(clippy::expect_used)]
        RuleId::try_new(s.to_owned()).expect("valid rule id in test")
    }

    fn chunk(id: &str, parent: Option<&str>, body: &str) -> RuleChunk {
        RuleChunk {
            id: rid(id),
            parent_id: parent.map(rid),
            subsection: rid(&id[..3]),
            heading: "H".into(),
            body: body.into(),
            examples: vec![],
            #[allow(clippy::expect_used)]
            cr_version: CrVersion::try_new("20260919".to_owned()).expect("valid CR version in test"),
        }
    }

    fn stored(id: &str, parent: Option<&str>, body: &str) -> StoredRule {
        StoredRule { id: rid(id), parent_id: parent.map(rid), body: body.into() }
    }

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<RuleId, RuleId> {
        pairs.iter().map(|(a, b)| (rid(a), rid(b))).collect()
    }

    #[test]
    fn inserted_rule_shifts_the_rest_and_cross_references_are_seen_through() {
        // Old: 702.19 Trample (refers to 702.20), 702.20 Vigilance. New: 702.19 Trample,
        // 702.20 NewKeyword, 702.21 Vigilance — and Trample's cross-reference moved too.
        let old = [
            stored("702.19", None, "702.19. Trample\n702.19a See rule 702.20."),
            stored("702.19a", Some("702.19"), "702.19a See rule 702.20."),
            stored("702.20", None, "702.20. Vigilance\n702.20a Vigilance is static."),
            stored("702.20a", Some("702.20"), "702.20a Vigilance is static."),
        ];
        let new = [
            chunk("702.19", None, "702.19. Trample\n702.19a See rule 702.21."),
            chunk("702.19a", Some("702.19"), "702.19a See rule 702.21."),
            chunk("702.20", None, "702.20. NewKeyword\n702.20a It is new."),
            chunk("702.20a", Some("702.20"), "702.20a It is new."),
            chunk("702.21", None, "702.21. Vigilance\n702.21a Vigilance is static."),
            chunk("702.21a", Some("702.21"), "702.21a Vigilance is static."),
        ];
        assert_eq!(renumber_map(&old, &new), m(&[("702.20", "702.21"), ("702.20a", "702.21a")]));
    }

    #[test]
    fn changed_text_and_ambiguous_bodies_are_not_relocated() {
        let old = [
            stored("100.1", None, "100.1. Reworded later."),
            stored("100.2", None, "100.2. Twin."),
            stored("100.3", None, "100.3. Twin."),
        ];
        let new = [
            chunk("100.5", None, "100.5. Reworded now."),
            chunk("100.6", None, "100.6. Twin."),
            chunk("100.7", None, "100.7. Twin."),
        ];
        assert!(renumber_map(&old, &new).is_empty());
    }

    #[test]
    fn a_leaf_inserted_mid_list_moves_the_later_letters() {
        let old = [
            stored("702.19", None, "702.19. Trample\n702.19a First.\n702.19b Second."),
            stored("702.19a", Some("702.19"), "702.19a First."),
            stored("702.19b", Some("702.19"), "702.19b Second."),
        ];
        let new = [
            chunk("702.19", None, "702.19. Trample\n702.19a First.\n702.19b Inserted.\n702.19c Second."),
            chunk("702.19a", Some("702.19"), "702.19a First."),
            chunk("702.19b", Some("702.19"), "702.19b Inserted."),
            chunk("702.19c", Some("702.19"), "702.19c Second."),
        ];
        // The parent's body changed (a line was added), so the parent is not mapped
        // and the retirement pass decides about citations of 702.19 itself; the
        // untouched leaf still follows its letter.
        assert_eq!(renumber_map(&old, &new), m(&[("702.19b", "702.19c")]));
    }

    #[test]
    fn rewrite_is_single_pass_and_bounded_by_word_edges() {
        let map = m(&[("702.19", "702.20"), ("702.20", "702.21"), ("702.19b", "702.20b")]);
        assert_eq!(
            rewrite_ids("See 702.19 and 702.20; also 702.19b, not 1702.19 or 702.190.", &map).as_deref(),
            Some("See 702.20 and 702.21; also 702.20b, not 1702.19 or 702.190.")
        );
        assert_eq!(rewrite_ids("rule 704 and 702.5", &map), None);
    }

    /// `citations[i][key]` without indexing.
    fn at<'a>(v: &'a serde_json::Value, i: usize, key: &str) -> Option<&'a serde_json::Value> {
        v.get(i).and_then(|c| c.get(key))
    }

    #[test]
    fn rewrite_call_touches_rule_and_prior_call_quotes_only() -> anyhow::Result<()> {
        let map = m(&[("702.19b", "702.20b")]);
        let citations = serde_json::json!([
            {"kind": "rule", "id": "702.19b", "quote": "702.19b The controller of an attacking creature"},
            {"kind": "scryfall_ruling", "card": "00000000-0000-0000-0000-000000000007", "ruling": "0123456789abcdef", "quote": "see rule 702.19b"},
            {"kind": "oracle_text", "card": "00000000-0000-0000-0000-000000000007", "face": 0, "quote": "Trample (702.19b)"},
            {"kind": "prior_call", "id": "00000000-0000-0000-0000-000000000009", "quote": "Under 702.19b you assign lethal"},
            {"kind": "rule", "id": "", "quote": ""}
        ]);
        let (c, a) = rewrite_call(&citations, "Rule 702.19b says so.", &map).ok_or_else(|| anyhow::anyhow!("expected a change"))?;
        assert_eq!(a, "Rule 702.20b says so.");
        assert_eq!(at(&c, 0, "id"), Some(&serde_json::json!("702.20b")));
        assert_eq!(at(&c, 0, "quote"), Some(&serde_json::json!("702.20b The controller of an attacking creature")));
        assert_eq!(at(&c, 1, "quote"), Some(&serde_json::json!("see rule 702.19b")), "ruling text was not renumbered");
        assert_eq!(at(&c, 2, "quote"), Some(&serde_json::json!("Trample (702.19b)")), "Oracle text was not renumbered");
        assert_eq!(at(&c, 3, "quote"), Some(&serde_json::json!("Under 702.20b you assign lethal")), "answers are rewritten by the same map");
        assert_eq!(c.get(4), citations.get(4), "an unreadable element is carried through");
        assert_eq!(rewrite_call(&citations, "nothing here", &m(&[("100.1", "100.2")])), None);
        Ok(())
    }
}
