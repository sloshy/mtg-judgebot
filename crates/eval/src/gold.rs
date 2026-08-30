//! `eval/gold.yaml` schema (only the fields the evaluators use).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use judge_core::RuleId;
use serde::Deserialize;

/// The gold file.
#[derive(Debug, Deserialize)]
pub struct Gold {
    /// All questions, in file order.
    pub questions: Vec<GoldQuestion>,
}

/// One gold question.
#[derive(Debug, Deserialize)]
pub struct GoldQuestion {
    /// Stable id, e.g. `layers-blood-moon-tron`.
    pub id: String,
    /// The question as a user would write it.
    pub question: String,
    /// Full card names involved.
    #[serde(default)]
    pub cards: Vec<String>,
    /// Nicknames / partial names the question uses.
    #[serde(default)]
    pub nicknames_used: Vec<String>,
    /// Free-form category labels (mapped onto `judge_core::Category` best-effort).
    #[serde(default)]
    pub categories: Vec<String>,
    /// `CR`, `Commander`, `Tournament` or `OutOfScope`.
    pub source: String,
    /// Rule ids a correct answer must cite (rule or leaf granularity).
    #[serde(default)]
    pub expected_rule_ids: Vec<YamlScalar>,
}

impl GoldQuestion {
    /// Whether the bot is supposed to answer (and so retrieval is scored).
    #[must_use]
    pub fn is_answerable(&self) -> bool {
        matches!(self.source.to_ascii_lowercase().as_str(), "cr" | "commander")
    }
}

/// A YAML scalar that may have been written unquoted (`613.8` parses as a float).
///
/// An unquoted id is rejected by [`load`]: a float cannot round-trip a trailing
/// zero (`613.10` would silently become `613.1`), so ids must be quoted or
/// carry a letter suffix.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum YamlScalar {
    /// Quoted or letter-suffixed ids.
    Text(String),
    /// Unquoted `613.8`-style ids (rejected on load).
    Number(serde_yaml_ng::Number),
}

impl YamlScalar {
    /// The id as text.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            YamlScalar::Text(s) => s.trim().to_owned(),
            YamlScalar::Number(n) => n.to_string(),
        }
    }
}

/// Every expected id must be a quoted (or letter-suffixed) scalar that is a valid `RuleId`.
fn validate(gold: &Gold) -> anyhow::Result<()> {
    for q in &gold.questions {
        for id in &q.expected_rule_ids {
            if let YamlScalar::Number(n) = id {
                anyhow::bail!("question {}: expected_rule_id {n} is unquoted; write it as '{n}' (a float drops trailing zeros)", q.id);
            }
            RuleId::try_new(id.as_text()).with_context(|| format!("question {}: bad expected_rule_id {:?}", q.id, id.as_text()))?;
        }
    }
    Ok(())
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
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let gold: Gold = serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    validate(&gold)?;
    Ok(gold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gold(ids: &str) -> anyhow::Result<Gold> {
        Ok(serde_yaml_ng::from_str(&format!("questions:\n- id: a\n  question: q\n  source: CR\n  expected_rule_ids: {ids}\n"))?)
    }

    #[test]
    fn unquoted_and_malformed_ids_are_rejected() -> anyhow::Result<()> {
        assert!(validate(&gold("['613.10', 614.1c, '704.5aa']")?).is_ok());
        let err = validate(&gold("[613.10]")?).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("unquoted"), "{err}");
        assert!(validate(&gold("['61.1']")?).is_err());
        Ok(())
    }

    #[test]
    fn shipped_gold_file_loads() {
        assert!(load(&default_path()).is_ok());
    }
}
