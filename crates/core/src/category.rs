//! `Category` is generated at build time from `data/categories.yaml`.

use std::{fmt, str::FromStr};

include!(concat!(env!("OUT_DIR"), "/category.rs"));

/// Error returned when a string is not a known category id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown category id: {0}")]
pub struct UnknownCategory(pub String);

impl FromStr for Category {
    type Err = UnknownCategory;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Category::ALL
            .iter()
            .copied()
            .find(|c| c.id() == s)
            .ok_or_else(|| UnknownCategory(s.to_owned()))
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_has_roughly_25_entries() {
        assert!(
            (20..=30).contains(&Category::ALL.len()),
            "got {}",
            Category::ALL.len()
        );
    }

    #[test]
    fn parses_known_ids_and_roundtrips() -> Result<(), UnknownCategory> {
        let layers: Category = "layers".parse()?;
        assert_eq!(layers, Category::Layers);
        assert_eq!(layers.subsections(), &["613"]);
        for c in Category::ALL {
            assert_eq!(c.id().parse::<Category>()?, *c);
            assert_eq!(c.to_string(), c.id());
        }
        Ok(())
    }

    #[test]
    fn rejects_unknown_id() {
        assert_eq!(
            "banana".parse::<Category>(),
            Err(UnknownCategory("banana".into()))
        );
    }

    #[test]
    fn serde_uses_snake_case_id() -> Result<(), serde_json::Error> {
        let json = serde_json::to_string(&Category::ReplacementEffects)?;
        assert_eq!(json, "\"replacement_effects\"");
        let back: Category = serde_json::from_str(&json)?;
        assert_eq!(back, Category::ReplacementEffects);
        assert!(serde_json::from_str::<Category>("\"nope\"").is_err());
        Ok(())
    }

    #[test]
    fn subsection_ids_are_valid_rule_ids() {
        for c in Category::ALL {
            for s in c.subsections() {
                assert!(
                    crate::RuleId::try_new((*s).to_owned()).is_ok(),
                    "{c}: bad subsection {s}"
                );
            }
        }
    }
}
