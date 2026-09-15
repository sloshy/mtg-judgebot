//! Best-effort mapping from the gold set's free-form category labels onto
//! `judge_core::Category`. The gold labels predate the taxonomy; unmapped
//! labels are logged so the table can grow.

use judge_core::Category;

/// Synonyms, keyed by the normalized label (lowercase, `_`-separated).
const SYNONYMS: &[(&str, Category)] = &[
    ("enters_the_battlefield", Category::ReplacementEffects),
    ("continuous_effects", Category::StaticAbilities),
    ("type_changing", Category::Layers),
    ("dependency", Category::Layers),
    ("timestamps", Category::Layers),
    (
        "characteristic_defining_abilities",
        Category::StaticAbilities,
    ),
    ("copy_effects", Category::Copying),
    ("copy", Category::Copying),
    ("lands", Category::CardTypesAndCharacteristics),
    ("characteristics", Category::CardTypesAndCharacteristics),
    (
        "card_characteristics",
        Category::CardTypesAndCharacteristics,
    ),
    ("mana_value", Category::CardTypesAndCharacteristics),
    ("errata_oracle_text", Category::GameConcepts),
    ("errata", Category::GameConcepts),
    ("multi_face_cards", Category::MultiFacedCards),
    ("multi_faced_cards", Category::MultiFacedCards),
    ("double_faced_cards", Category::MultiFacedCards),
    ("zone_changes", Category::Zones),
    ("exile", Category::Zones),
    ("graveyard", Category::Zones),
    ("command_zone", Category::Commander),
    ("commander_damage", Category::Commander),
    ("color_identity", Category::ColorsAndColorIdentity),
    ("colors", Category::ColorsAndColorIdentity),
    ("mana", Category::ManaAbilities),
    ("timing", Category::StackAndPriority),
    ("priority", Category::StackAndPriority),
    ("stack", Category::StackAndPriority),
    ("legend_rule", Category::StateBasedActions),
    ("combat_damage", Category::Combat),
    ("keywords", Category::KeywordAbilities),
    ("keyword", Category::KeywordAbilities),
    ("damage", Category::DamageAndLife),
    ("life", Category::DamageAndLife),
    ("lifelink", Category::DamageAndLife),
    ("deathtouch", Category::DamageAndLife),
    ("targets", Category::Targeting),
    ("tokens", Category::Copying),
];

/// Normalize a gold label: lowercase, punctuation and spaces → `_`.
fn normalize(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut last_sep = true;
    for c in label.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            out.push('_');
            last_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Map one label. Tries the exact `Category` id first, then synonyms.
#[must_use]
pub fn map_label(label: &str) -> Option<Category> {
    let key = normalize(label);
    key.parse::<Category>()
        .ok()
        .or_else(|| SYNONYMS.iter().find(|(k, _)| *k == key).map(|(_, c)| *c))
}

/// Map all labels of a question, deduplicated and in order; logs the unmapped ones.
#[must_use]
pub fn map_labels(question_id: &str, labels: &[String]) -> Vec<Category> {
    let mut out: Vec<Category> = Vec::new();
    for label in labels {
        match map_label(label) {
            Some(c) if !out.contains(&c) => out.push(c),
            Some(_) => {}
            None => tracing::warn!(
                question = question_id,
                label,
                "gold category has no Category mapping"
            ),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ids_and_synonyms() {
        assert_eq!(map_label("layers"), Some(Category::Layers));
        assert_eq!(
            map_label("replacement-effects"),
            Some(Category::ReplacementEffects)
        );
        assert_eq!(map_label("casting spells"), Some(Category::CastingSpells));
        assert_eq!(
            map_label("state-based-actions"),
            Some(Category::StateBasedActions)
        );
        assert_eq!(
            map_label("multi-face-cards"),
            Some(Category::MultiFacedCards)
        );
        assert_eq!(map_label("not a rules question"), None);
    }
}
