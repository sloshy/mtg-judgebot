//! Scoring for the `answer` subcommand: citation recall and source match.

use std::collections::BTreeSet;

use judge_core::Citation;

/// The rule-granularity id of `id`: `613.1d` → `613.1`; `613.1` and `613` unchanged.
#[must_use]
pub fn rule_of(id: &str) -> &str {
    id.trim_end_matches(|c: char| c.is_ascii_alphabetic())
}

/// Whether `cited` covers `expected`: exact, or a leaf covers its parent rule
/// (`613.1d` cites `613.1`) and a parent covers its leaves (`613.1` cites `613.1d`).
#[must_use]
pub fn covers(cited: &str, expected: &str) -> bool {
    cited == expected || rule_of(cited) == expected || cited == rule_of(expected)
}

/// Rule ids from the citations, in order and deduplicated.
#[must_use]
pub fn cited_rule_ids(citations: &[Citation]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for c in citations {
        if let Citation::Rule { id, .. } = c {
            let s: &str = id.as_ref();
            if seen.insert(s.to_owned()) {
                out.push(s.to_owned());
            }
        }
    }
    out
}

/// How many citations of each kind a verdict carried.
#[expect(
    clippy::struct_field_names,
    reason = "the field names are the keys in persisted run JSON (eval/runs/*.json)"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CiteCounts {
    /// `Citation::Rule` entries.
    pub n_rule_cites: usize,
    /// `Citation::ScryfallRuling` entries.
    pub n_ruling_cites: usize,
    /// `Citation::OracleText` entries.
    pub n_oracle_cites: usize,
}

/// Count rule, ruling and Oracle-text citations (prior-call citations are none of these).
#[must_use]
pub fn cite_counts(citations: &[Citation]) -> CiteCounts {
    citations.iter().fold(CiteCounts::default(), |mut n, c| {
        match c {
            Citation::Rule { .. } => n.n_rule_cites += 1,
            Citation::ScryfallRuling { .. } => n.n_ruling_cites += 1,
            Citation::OracleText { .. } => n.n_oracle_cites += 1,
            Citation::PriorCall { .. } => {}
        }
        n
    })
}

/// Citation recall for one question.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recall {
    /// Expected ids that some citation covers.
    pub hit: Vec<String>,
    /// Expected ids nothing covers.
    pub missed: Vec<String>,
}

impl Recall {
    /// Whether at least one expected id was cited.
    #[must_use]
    pub fn any_hit(&self) -> bool {
        !self.hit.is_empty()
    }
    /// Whether anything was expected at all (an unanswerable question, or one with no ids, expects nothing).
    #[must_use]
    pub fn expects_any(&self) -> bool {
        !(self.hit.is_empty() && self.missed.is_empty())
    }
}

/// Alternate ids per expected id (see `GoldQuestion::equivalent_rule_ids`).
pub type Equivalents = std::collections::BTreeMap<String, Vec<String>>;

/// Like [`recall`], but an expected id is also hit when a citation covers one
/// of its listed equivalent ids — the same fact stated elsewhere in the CR.
#[must_use]
pub fn recall_with(cited: &[String], expected: &[String], equivalents: &Equivalents) -> Recall {
    let satisfies = |c: &String, e: &String| {
        covers(c, e)
            || equivalents
                .get(e.as_str())
                .is_some_and(|alts| alts.iter().any(|a| covers(c, a)))
    };
    let (mut hit, mut missed) = (Vec::new(), Vec::new());
    for e in expected {
        if cited.iter().any(|c| satisfies(c, e)) {
            hit.push(e.clone());
        } else {
            missed.push(e.clone());
        }
    }
    Recall { hit, missed }
}

/// The gold `source` label as it would appear from the model (`snake_case`).
#[must_use]
pub fn normalize_source(label: &str) -> String {
    match label.to_ascii_lowercase().as_str() {
        "cr" => "cr".into(),
        "commander" => "commander".into(),
        "tournament" => "tournament".into(),
        "outofscope" | "out_of_scope" => "out_of_scope".into(),
        other => other.to_owned(),
    }
}

/// Whether a gold source label matches a pipeline `Source`.
#[must_use]
pub fn source_matches(label: &str, actual: judge_core::Source) -> bool {
    let actual = serde_json::to_value(actual)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    normalize_source(label) == actual
}

#[cfg(test)]
mod tests {
    /// Equivalence-free shorthand used by the pre-existing tests.
    fn recall(cited: &[String], expected: &[String]) -> Recall {
        recall_with(cited, expected, &Equivalents::new())
    }

    #[test]
    fn equivalents_satisfy_an_expected_id() {
        let eq = Equivalents::from([(
            "707.2".to_owned(),
            vec!["613.1a".to_owned(), "613.2c".to_owned()],
        )]);
        // Direct equivalent, leaf-of-equivalent, and an untouched miss.
        let r = recall_with(&v(&["613.1a"]), &v(&["707.2", "108.1"]), &eq);
        assert_eq!(r.hit, v(&["707.2"]));
        assert_eq!(r.missed, v(&["108.1"]));
        let r = recall_with(&v(&["613.2c"]), &v(&["707.2"]), &eq);
        assert!(r.missed.is_empty());
        // Equivalents never leak onto other expected ids.
        let r = recall_with(&v(&["613.1a"]), &v(&["616.1"]), &eq);
        assert_eq!(r.missed, v(&["616.1"]));
    }

    use super::*;
    use judge_core::{RuleId, Source};

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn leaf_and_parent_cover_each_other() {
        assert!(covers("613.1d", "613.1"));
        assert!(covers("613.1", "613.1d"));
        assert!(covers("613.1", "613.1"));
        assert!(!covers("613.1d", "613.1e"));
        assert!(!covers("613.1", "613.2"));
        assert!(!covers("613", "613.1"));
        assert!(!covers("704.5aa", "704.5a"));
        assert!(covers("704.5aa", "704.5"));
    }

    #[test]
    fn recall_counts_hits_over_expected() {
        let r = recall(
            &v(&["614.1c", "616.1", "999.9"]),
            &v(&["614.1", "616.1e", "614.5"]),
        );
        assert_eq!(r.hit, v(&["614.1", "616.1e"]));
        assert_eq!(r.missed, v(&["614.5"]));
        assert!(recall(&v(&["1.1"]), &[]).hit.is_empty());
    }

    #[test]
    fn cited_ids_come_from_rule_citations_only() -> anyhow::Result<()> {
        let c = vec![
            Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: judge_core::Quote::try_new("x")?,
            },
            Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: judge_core::Quote::try_new("y")?,
            },
            Citation::PriorCall {
                id: judge_core::CallId::new(uuid_nil()),
                quote: judge_core::Quote::try_new("z")?,
            },
        ];
        assert_eq!(cited_rule_ids(&c), v(&["702.15b"]));
        assert_eq!(
            recall(&cited_rule_ids(&c), &v(&["702.15"])).hit,
            v(&["702.15"])
        );
        Ok(())
    }

    #[test]
    fn cite_counts_by_kind() -> anyhow::Result<()> {
        let c = vec![
            Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: judge_core::Quote::try_new("x")?,
            },
            Citation::ScryfallRuling {
                card: judge_core::CardId::new(uuid_nil()),
                ruling: judge_core::ruling_key("2020-01-01", "a"),
                quote: judge_core::Quote::try_new("y")?,
            },
            Citation::ScryfallRuling {
                card: judge_core::CardId::new(uuid_nil()),
                ruling: judge_core::ruling_key("2020-01-01", "b"),
                quote: judge_core::Quote::try_new("y")?,
            },
            Citation::PriorCall {
                id: judge_core::CallId::new(uuid_nil()),
                quote: judge_core::Quote::try_new("z")?,
            },
            Citation::OracleText {
                card: judge_core::CardId::new(uuid_nil()),
                face: 0,
                quote: judge_core::Quote::try_new("w")?,
            },
        ];
        assert_eq!(
            cite_counts(&c),
            CiteCounts {
                n_rule_cites: 1,
                n_ruling_cites: 2,
                n_oracle_cites: 1
            }
        );
        assert_eq!(cite_counts(&[]), CiteCounts::default());
        let r = recall(&v(&["702.15b"]), &v(&["702.15", "1.1"]));
        assert!(r.any_hit() && r.expects_any());
        assert!(!recall(&[], &[]).expects_any());
        Ok(())
    }

    fn uuid_nil() -> uuid::Uuid {
        uuid::Uuid::nil()
    }

    #[test]
    fn sources_match_case_insensitively() {
        assert!(source_matches("CR", Source::Cr));
        assert!(source_matches("OutOfScope", Source::OutOfScope));
        assert!(source_matches("Tournament", Source::Tournament));
        assert!(!source_matches("CR", Source::Commander));
    }
}
