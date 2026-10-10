//! `eval/gold.yaml` schema. An unknown key is an error, so a misspelled list
//! is never read as an empty one.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use judge_core::{Category, CategoryGuess, Confidence, Extraction, RuleId, Source};
use serde::Deserialize;

use crate::categories;

/// The gold file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gold {
    /// Provenance notes; not read.
    #[serde(default, rename = "_meta")]
    _meta: serde_yaml_ng::Value,
    /// All questions, in file order.
    pub questions: Vec<GoldQuestion>,
}

/// How much an expected rule id counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    /// A correct answer must cite it; recall is scored on these.
    Decisive,
    /// Background a good answer may leave out: reported when cited, never a miss.
    Supporting,
}

/// One rule id a gold question expects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    /// The id (rule or leaf granularity).
    pub id: RuleId,
    /// Decisive or supporting.
    pub weight: Weight,
    /// Other ids stating the same fact, any of which satisfies this one.
    pub equivalents: Vec<RuleId>,
}

/// One gold question, validated: made only from [`RawQuestion`], so every
/// id parses, no id is both decisive and supporting, and every equivalent
/// hangs off an expected id.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawQuestion")]
pub struct GoldQuestion {
    /// Stable id, e.g. `layers-blood-moon-tron`.
    pub id: String,
    /// The question as a user would write it.
    pub question: String,
    /// Full card names involved.
    pub cards: Vec<String>,
    /// Nicknames / partial names the question uses.
    pub nicknames_used: Vec<String>,
    /// Free-form category labels (mapped onto `judge_core::Category` best-effort).
    pub categories: Vec<String>,
    /// `CR`, `Commander`, `Tournament` or `OutOfScope`.
    pub source: String,
    /// Expected rule ids, decisive first, each in file order.
    pub expected: Vec<Expected>,
    /// The reference answer, for side-by-side human review.
    pub expected_answer: String,
}

/// A gold question as written in YAML.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuestion {
    id: String,
    question: String,
    #[serde(default)]
    cards: Vec<String>,
    #[serde(default)]
    nicknames_used: Vec<String>,
    #[serde(default)]
    categories: Vec<String>,
    source: String,
    /// Rule ids a correct answer must cite.
    #[serde(default)]
    decisive_rule_ids: Vec<YamlScalar>,
    /// Rule ids a good answer may leave out.
    #[serde(default)]
    supporting_rule_ids: Vec<YamlScalar>,
    #[serde(default)]
    expected_answer: String,
    /// Alternate rule ids that also satisfy an expected id: the same fact
    /// stated elsewhere in the CR (e.g. `"707.2": ["613.2c"]`). Keys must be
    /// quoted and name a decisive or supporting id.
    #[serde(default)]
    equivalent_rule_ids: std::collections::BTreeMap<String, Vec<YamlScalar>>,
    /// Why the question is in the set; not read.
    #[serde(default, rename = "rationale")]
    _rationale: Option<String>,
    /// The verifier's audit note; not read.
    #[serde(default, rename = "verifier_note")]
    _verifier_note: Option<String>,
}

/// A quoted (or letter-suffixed) scalar that is a valid `RuleId`. A float
/// cannot round-trip a trailing zero (`613.10` would silently become
/// `613.1`), so an unquoted id is rejected.
fn rule_id(q: &str, what: &str, s: &YamlScalar) -> anyhow::Result<RuleId> {
    if let YamlScalar::Number(n) = s {
        anyhow::bail!(
            "question {q}: {what} {n} is unquoted; write it as '{n}' (a float drops trailing zeros)"
        );
    }
    RuleId::try_new(s.as_text())
        .with_context(|| format!("question {q}: bad {what} {:?}", s.as_text()))
}

impl TryFrom<RawQuestion> for GoldQuestion {
    type Error = anyhow::Error;

    fn try_from(raw: RawQuestion) -> anyhow::Result<Self> {
        let q = raw.id.as_str();
        let mut expected: Vec<Expected> = Vec::new();
        for (list, weight, what) in [
            (&raw.decisive_rule_ids, Weight::Decisive, "decisive rule id"),
            (
                &raw.supporting_rule_ids,
                Weight::Supporting,
                "supporting rule id",
            ),
        ] {
            for s in list {
                let id = rule_id(q, what, s)?;
                anyhow::ensure!(
                    !expected.iter().any(|e| e.id == id),
                    "question {q}: rule id {id} is listed twice (an id is in one list, once)"
                );
                expected.push(Expected {
                    id,
                    weight,
                    equivalents: Vec::new(),
                });
            }
        }
        for (k, alts) in &raw.equivalent_rule_ids {
            let key = RuleId::try_new(k.trim().to_owned())
                .with_context(|| format!("question {q}: bad equivalent_rule_ids key {k:?}"))?;
            let alts = alts
                .iter()
                .map(|a| rule_id(q, &format!("equivalent id under {key}"), a))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let Some(e) = expected.iter_mut().find(|e| e.id == key) else {
                anyhow::bail!(
                    "question {q}: equivalent_rule_ids key {key} is not a decisive or supporting rule id"
                );
            };
            anyhow::ensure!(
                e.equivalents.is_empty(),
                "question {q}: equivalent_rule_ids key {key} is listed twice"
            );
            e.equivalents = alts;
        }
        Ok(Self {
            id: raw.id,
            question: raw.question,
            cards: raw.cards,
            nicknames_used: raw.nicknames_used,
            categories: raw.categories,
            source: raw.source,
            expected,
            expected_answer: raw.expected_answer,
        })
    }
}

impl GoldQuestion {
    /// Expected ids of one weight, as text, in file order.
    #[must_use]
    pub fn ids(&self, weight: Weight) -> Vec<String> {
        self.expected
            .iter()
            .filter(|e| e.weight == weight)
            .map(|e| e.id.to_string())
            .collect()
    }

    /// Every expected id, decisive first, as text.
    #[must_use]
    pub fn all_ids(&self) -> Vec<String> {
        self.expected.iter().map(|e| e.id.to_string()).collect()
    }

    /// Equivalent ids per expected id, as text, for scoring.
    #[must_use]
    pub fn equivalents(&self) -> crate::score::Equivalents {
        self.expected
            .iter()
            .filter(|e| !e.equivalents.is_empty())
            .map(|e| {
                (
                    e.id.to_string(),
                    e.equivalents.iter().map(ToString::to_string).collect(),
                )
            })
            .collect()
    }

    /// The gold `source` label as a `Source` (anything unknown is out of scope).
    #[must_use]
    pub fn source(&self) -> Source {
        match self.source.to_ascii_lowercase().as_str() {
            "cr" => Source::Cr,
            "commander" => Source::Commander,
            "tournament" => Source::Tournament,
            _ => Source::OutOfScope,
        }
    }

    /// Whether the bot is supposed to answer (and so retrieval is scored).
    #[must_use]
    pub fn is_answerable(&self) -> bool {
        self.source().is_answerable()
    }

    /// The `Extraction` a perfect extractor would produce from the gold
    /// fields: full card names as spans, category labels as concepts and
    /// (mapped best-effort) as medium-confidence guesses, the first as
    /// `primary` (`other` at low confidence if none maps). Shared by the
    /// `recall` evaluator and the `--gold-extraction` answer run.
    #[must_use]
    pub fn extraction(&self) -> Extraction {
        let mut mapped = categories::map_labels(&self.id, &self.categories).into_iter();
        let primary = mapped.next().map_or(
            CategoryGuess {
                category: Category::Other,
                confidence: Confidence::Low,
            },
            |category| CategoryGuess {
                category,
                confidence: Confidence::Medium,
            },
        );
        let secondary = mapped
            .take(Extraction::MAX_SECONDARY)
            .map(|category| CategoryGuess {
                category,
                confidence: Confidence::Medium,
            })
            .collect();
        Extraction {
            card_spans: self.cards.clone(),
            concepts: self
                .categories
                .iter()
                .map(|c| c.replace(['-', '_'], " "))
                .collect(),
            primary,
            secondary,
            source: self.source(),
        }
    }
}

/// A YAML scalar that may have been written unquoted (`613.8` parses as a
/// float). Only [`rule_id`] reads one, and it rejects the unquoted form.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum YamlScalar {
    /// Quoted or letter-suffixed ids.
    Text(String),
    /// Unquoted `613.8`-style ids (rejected on load).
    Number(serde_yaml_ng::Number),
}

impl YamlScalar {
    /// The id as text.
    fn as_text(&self) -> String {
        match self {
            YamlScalar::Text(s) => s.trim().to_owned(),
            YamlScalar::Number(n) => n.to_string(),
        }
    }
}

/// `eval/gold.yaml` relative to the working directory, else relative to the workspace.
#[must_use]
pub fn default_path() -> PathBuf {
    let cwd = PathBuf::from("eval/gold.yaml");
    if cwd.exists() {
        return cwd;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/gold.yaml")
}

/// Parse the gold file.
///
/// # Errors
/// If the file cannot be read or does not match the schema.
pub fn load(path: &Path) -> anyhow::Result<Gold> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(fields: &str) -> anyhow::Result<GoldQuestion> {
        Ok(serde_yaml_ng::from_str(&format!(
            "id: a\nquestion: q\nsource: CR\n{fields}"
        ))?)
    }

    fn err(fields: &str) -> String {
        question(fields)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default()
    }

    #[test]
    fn unquoted_and_malformed_ids_are_rejected() -> anyhow::Result<()> {
        let q =
            question("decisive_rule_ids: ['613.10', 614.1c]\nsupporting_rule_ids: ['704.5aa']\n")?;
        assert_eq!(q.ids(Weight::Decisive), vec!["613.10", "614.1c"]);
        assert_eq!(q.ids(Weight::Supporting), vec!["704.5aa"]);
        assert!(err("decisive_rule_ids: [613.10]\n").contains("unquoted"));
        assert!(err("supporting_rule_ids: [613.10]\n").contains("unquoted"));
        assert!(!err("decisive_rule_ids: ['61.1']\n").is_empty());
        Ok(())
    }

    #[test]
    fn the_lists_are_disjoint_and_equivalents_hang_off_either() -> anyhow::Result<()> {
        assert!(
            err("decisive_rule_ids: ['702.19']\nsupporting_rule_ids: ['702.19']\n")
                .contains("listed twice")
        );
        let q = question(
            "decisive_rule_ids: ['707.2']\nsupporting_rule_ids: [704.6c]\n\
             equivalent_rule_ids:\n  '707.2': [613.2c]\n  704.6c: [903.10a]\n",
        )?;
        assert_eq!(q.equivalents().len(), 2);
        assert!(
            err("decisive_rule_ids: ['707.2']\nequivalent_rule_ids:\n  '702.19': [702.19b]\n")
                .contains("is not a decisive or supporting")
        );
        assert!(
            err("decisive_rule_ids: ['707.2']\nequivalent_rule_ids:\n  '707.2': [613.20]\n")
                .contains("unquoted")
        );
        // Two keys naming one id would silently drop one list of alternates.
        assert!(
            err("decisive_rule_ids: ['707.2']\nequivalent_rule_ids:\n  '707.2': [613.2c]\n  ' 707.2': [613.2a]\n")
                .contains("is listed twice")
        );
        Ok(())
    }

    /// The old single list, or a misspelled key, is an error rather than
    /// silently expecting nothing.
    #[test]
    fn unknown_keys_are_rejected() {
        assert!(err("expected_rule_ids: ['702.19']\n").contains("unknown field"));
        assert!(err("decisive_rule_id: ['702.19']\n").contains("unknown field"));
    }

    #[test]
    fn shipped_gold_file_loads() -> anyhow::Result<()> {
        let gold = load(&default_path())?;
        let decisive: usize = gold
            .questions
            .iter()
            .map(|q| q.ids(Weight::Decisive).len())
            .sum();
        let supporting: usize = gold
            .questions
            .iter()
            .map(|q| q.ids(Weight::Supporting).len())
            .sum();
        assert_eq!((decisive, supporting), (37, 33));
        Ok(())
    }
}
