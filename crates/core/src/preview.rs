//! Cards that are not out yet (D42).
//!
//! Scryfall lists a card as soon as it is previewed, weeks before its set
//! releases. The Comprehensive Rules update for a set is published about a week
//! before the release (prerelease), so until then a previewed card's new
//! mechanics, keywords and creature types are missing from the rules the judge
//! answers from. The CR does not name the sets it covers, so coverage is judged
//! from two signals:
//!
//! - **The CR's date**: the loaded CR must be dated no more than
//!   [`COVER_WINDOW_DAYS`] before the card's first printing. An older one
//!   predates the set's update.
//! - **The card's rules terms** ([`rules_terms`]): every card type, subtype
//!   and keyword on the card must appear somewhere in the loaded CR.
//!
//! The dates and the CR text are read by the adapter. This module decides
//! what they mean.

use nonempty::NonEmpty;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Face;

/// How long before a card's first printing the loaded CR may be dated and
/// still be taken for that set's update. Wizards dates a set's CR about a week
/// before release (the Hobbit CR on 2026-08-07 for a 2026-08-14 release,
/// Reality Fracture's on 2026-09-25 for 2026-10-02). Three weeks leaves room
/// for an early publication without reaching back to the previous set's update.
pub const COVER_WINDOW_DAYS: i32 = 21;

/// Whether a card is out yet, on the day a question is answered.
///
/// `Upcoming` is boxed: a `Card` sits inline in `JudgeError`'s ambiguity
/// variant, and a released card, almost every one, should cost a pointer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Release {
    /// First printed on or before today, or no printing date is known.
    #[default]
    Released,
    /// First printed after today: previewed, not out.
    Upcoming(Box<Upcoming>),
}

/// A card that is not out yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Upcoming {
    /// Its first printing's date, `YYYY-MM-DD`.
    pub on: String,
    /// Whether the loaded CR has its rules.
    pub coverage: Coverage,
}

impl Release {
    /// The status from what the adapter read. `upcoming_on` is the first
    /// printing's date when that is after today; `cr_predates` says the
    /// loaded CR is older than the [`COVER_WINDOW_DAYS`] before it, and
    /// `unknown` lists the card's [`rules_terms`] the loaded CR never
    /// mentions. Both are ignored for a released card.
    #[must_use]
    pub fn assess(upcoming_on: Option<String>, cr_predates: bool, unknown: Vec<String>) -> Self {
        let Some(on) = upcoming_on else {
            return Self::Released;
        };
        let gaps = cr_predates
            .then_some(Gap::CrPredatesSet)
            .into_iter()
            .chain(unknown.into_iter().map(|term| Gap::UnknownTerm { term }));
        let coverage = match NonEmpty::collect(gaps) {
            Some(gaps) => Coverage::Uncovered { gaps },
            None => Coverage::Covered,
        };
        Self::Upcoming(Box::new(Upcoming { on, coverage }))
    }

    /// What a reply says about the card: see [`CardStatus`].
    #[must_use]
    pub fn status(&self) -> CardStatus {
        match self {
            Self::Released => CardStatus::Released,
            Self::Upcoming(u) => match u.coverage {
                Coverage::Covered => CardStatus::Previewed,
                Coverage::Uncovered { .. } => CardStatus::Unreleased,
            },
        }
    }
}

/// Whether the loaded CR has an upcoming card's rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "coverage", rename_all = "snake_case", deny_unknown_fields)]
pub enum Coverage {
    /// The loaded CR is the card's set's update, and mentions every rules
    /// term on it: prerelease week, with the rules out.
    Covered,
    /// The loaded CR may not have the card's rules, for these reasons.
    Uncovered {
        /// At least one.
        #[schemars(with = "Vec<Gap>")]
        gaps: NonEmpty<Gap>,
    },
}

/// One reason the loaded CR may not have an upcoming card's rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "gap", rename_all = "snake_case", deny_unknown_fields)]
pub enum Gap {
    /// The loaded CR is dated more than [`COVER_WINDOW_DAYS`] before the
    /// card's first printing, so it predates the card's set's update.
    CrPredatesSet,
    /// A type, subtype or keyword on the card that the loaded CR never mentions.
    UnknownTerm {
        /// As the card prints it.
        term: String,
    },
}

/// A card's release status as a reply shows it, and as a stored call keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    /// Out.
    #[default]
    Released,
    /// Not out, but the loaded CR is its set's update and knows its terms.
    Previewed,
    /// Not out, and the loaded CR may not cover it: the reply carries the
    /// unreleased-card note.
    Unreleased,
}

impl CardStatus {
    /// Whether a reply carries the unreleased-card note for this card.
    #[must_use]
    pub const fn noted(self) -> bool {
        matches!(self, Self::Unreleased)
    }
}

/// A rules term on a card: a word the CR must mention for the card's rules to
/// be in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    /// As the card prints it: `Vulcan`, `Face a dilemma`.
    pub name: String,
    /// A word the CR defines the term's family under, which also counts as
    /// mentioning it: `typecycling` for `Halflingcycling`, `landwalk` for
    /// `Desertwalk`.
    pub family: Option<&'static str>,
}

/// Keyword families the CR defines once for every instance (702.29 and
/// 702.14): a suffix and the family's name.
const FAMILIES: &[(&str, &str)] = &[("cycling", "typecycling"), ("walk", "landwalk")];

/// The words on a card that the CR must mention for its rules to be in it:
/// every card type, supertype and subtype on its faces' type lines, and every
/// Scryfall keyword except ability and flavor words.
///
/// An ability or flavor word ("Landfall —", "Strange new worlds —") has no
/// rules meaning (CR 207.2c, 207.2d), and a new set's flavor words never reach
/// the CR, so a keyword that some face's Oracle text uses only as a line's
/// italic prefix is left out. Terms are deduplicated case-insensitively, in
/// the card's order.
#[must_use]
pub fn rules_terms(keywords: &[String], faces: &[Face]) -> Vec<Term> {
    let mut out: Vec<Term> = Vec::new();
    let mut push = |name: &str| {
        let name = name.trim();
        if name.is_empty() || out.iter().any(|t| t.name.eq_ignore_ascii_case(name)) {
            return;
        }
        let lower = name.to_lowercase();
        let family = FAMILIES
            .iter()
            .find(|(suffix, _)| lower.len() > suffix.len() && lower.ends_with(suffix))
            .map(|(_, family)| *family);
        out.push(Term {
            name: name.to_owned(),
            family,
        });
    };
    for face in faces {
        for half in face.type_line.split("//") {
            for word in half.split(['—', ' ']) {
                push(word);
            }
        }
    }
    for k in keywords {
        if !only_as_ability_word(k, faces) {
            push(k);
        }
    }
    out
}

/// Whether every use of `keyword` in the faces' Oracle text is a line's
/// `Keyword — ` prefix (after a mode's bullet, if any), which is how ability
/// words, flavor words and mode names print. A keyword the text never
/// mentions is not one.
fn only_as_ability_word(keyword: &str, faces: &[Face]) -> bool {
    let k = keyword.to_lowercase();
    let mut prefixed = false;
    for face in faces {
        for line in face.oracle_text.lines() {
            // A mode's name prints after its bullet: `• Take the Elevator — …`.
            let line = line.trim_start();
            let line = line
                .strip_prefix('•')
                .unwrap_or(line)
                .trim_start()
                .to_lowercase();
            let Some(rest) = line.strip_prefix(&k) else {
                if line.contains(&k) {
                    return false;
                }
                continue;
            };
            let after = rest.trim_start().strip_prefix('—');
            match after.map(str::trim_start) {
                // `Exhaust — {6}: …`, `Ward—Pay 3 life.`: a keyword with its
                // cost, not an ability word.
                Some(effect) if effect.starts_with('{') || effect.starts_with("pay ") => {
                    return false;
                }
                Some(effect) if !effect.contains(&k) => prefixed = true,
                _ => return false,
            }
        }
    }
    prefixed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(type_line: &str, text: &str) -> Face {
        Face {
            name: "X".into(),
            oracle_text: text.into(),
            mana_cost: String::new(),
            type_line: type_line.into(),
        }
    }

    fn names(terms: &[Term]) -> Vec<&str> {
        terms.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn types_and_rules_keywords_are_terms_and_ability_words_are_not() {
        let spock = face(
            "Legendary Creature — Vulcan Human Scientist",
            "Strange new worlds — Whenever Commander Spock enters or attacks, reveal the top card.",
        );
        assert_eq!(
            names(&rules_terms(&["Strange new worlds".into()], &[spock])),
            ["Legendary", "Creature", "Vulcan", "Human", "Scientist"]
        );

        let seven = face(
            "Legendary Artifact Creature — Human Scientist",
            "Whenever you face a dilemma, draw a card. (You face a dilemma as you choose one or more modes for a spell or ability.)",
        );
        assert_eq!(
            names(&rules_terms(&["Face a dilemma".into()], &[seven])),
            [
                "Legendary",
                "Artifact",
                "Creature",
                "Human",
                "Scientist",
                "Face a dilemma"
            ]
        );
    }

    #[test]
    fn a_keyword_with_its_cost_after_a_dash_is_a_term() {
        let exhaust = face(
            "Artifact Creature — Construct",
            "Exhaust — {6}: Put three +1/+1 counters on this creature.",
        );
        assert!(names(&rules_terms(&["Exhaust".into()], &[exhaust])).contains(&"Exhaust"));
        let ward = face("Creature — Spirit", "Ward—Pay 3 life.");
        assert!(names(&rules_terms(&["Ward".into()], &[ward])).contains(&"Ward"));
    }

    #[test]
    fn a_mode_name_is_not_a_term() {
        let f = face(
            "Sorcery",
            "Choose one —\n• Take the Elevator — Draw a card.\n• Take 59 Flights of Stairs — Scry 2.",
        );
        assert_eq!(
            names(&rules_terms(
                &[
                    "Take the Elevator".into(),
                    "Take 59 Flights of Stairs".into()
                ],
                &[f]
            )),
            ["Sorcery"]
        );
    }

    #[test]
    fn a_keyword_used_both_ways_is_a_term() {
        // "Landfall" as an ability word and in running text: the running use
        // means the CR has to know the word.
        let f = face(
            "Creature — Elf",
            "Landfall — Whenever a land you control enters, draw.\nYour spells with landfall cost {1} less.",
        );
        assert!(names(&rules_terms(&["Landfall".into()], &[f])).contains(&"Landfall"));
    }

    #[test]
    fn faces_split_and_duplicates_collapse() {
        let front = face("Creature — Human Wizard", "");
        let back = face("Creature — Human Werewolf", "");
        assert_eq!(
            names(&rules_terms(&[], &[front, back])),
            ["Creature", "Human", "Wizard", "Werewolf"]
        );
        let split = face("Instant // Sorcery", "");
        assert_eq!(names(&rules_terms(&[], &[split])), ["Instant", "Sorcery"]);
    }

    #[test]
    fn keyword_families_name_their_rule() {
        let f = face("Creature — Halfling", "Halflingcycling {2}");
        let terms = rules_terms(&["Halflingcycling".into(), "Cycling".into()], &[f]);
        let halfling = terms.iter().find(|t| t.name == "Halflingcycling");
        assert_eq!(halfling.and_then(|t| t.family), Some("typecycling"));
        // The family's own word is not one of its members.
        let cycling = terms.iter().find(|t| t.name == "Cycling");
        assert_eq!(cycling.and_then(|t| t.family), None);
    }

    #[test]
    fn a_status_follows_the_gaps() {
        assert_eq!(
            Release::assess(None, true, vec!["Vulcan".into()]),
            Release::Released
        );
        let covered = Release::assess(Some("2026-11-13".into()), false, vec![]);
        assert_eq!(covered.status(), CardStatus::Previewed);
        let old_cr = Release::assess(Some("2026-11-13".into()), true, vec![]);
        assert_eq!(old_cr.status(), CardStatus::Unreleased);
        let unknown = Release::assess(Some("2026-11-13".into()), false, vec!["Vulcan".into()]);
        assert_eq!(
            unknown,
            Release::Upcoming(Box::new(Upcoming {
                on: "2026-11-13".into(),
                coverage: Coverage::Uncovered {
                    gaps: NonEmpty::new(Gap::UnknownTerm {
                        term: "Vulcan".into()
                    })
                }
            }))
        );
        assert!(unknown.status().noted());
    }

    #[test]
    fn a_release_round_trips() -> anyhow::Result<()> {
        let r = Release::assess(Some("2026-11-13".into()), true, vec!["Borg".into()]);
        let json = serde_json::to_value(&r)?;
        assert_eq!(
            json,
            serde_json::json!({"status": "upcoming", "on": "2026-11-13", "coverage": {
                "coverage": "uncovered",
                "gaps": [{"gap": "cr_predates_set"}, {"gap": "unknown_term", "term": "Borg"}]}})
        );
        assert_eq!(serde_json::from_value::<Release>(json)?, r);
        Ok(())
    }
}
