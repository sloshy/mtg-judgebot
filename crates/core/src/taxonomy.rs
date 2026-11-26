//! The CR text a category always includes, named by title.
//!
//! `data/categories.yaml` names each section by its title (`Adventurer Cards`),
//! not its number: a release that inserts a section moves every later number,
//! and a moved number is still a valid section, so nothing would report it. The
//! numbers are looked up in the loaded CR ([`Outline::resolve`]) each time the
//! `categories` table is written. A title that matches nothing is
//! [`Unresolved`] and is left out, never pointed at another section.
//!
//! Three things can be named ([`Within`]): a whole section, one rule by its
//! title (`Keyword Abilities` > `Lifelink`), and one rule or sub-rule that has
//! no title, by its number inside the titled section (`Commander` > `.5`).
//! `General` is the title of the first section of every part, so it is
//! qualified by the part's title (`Zones` > `General`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Category;

/// One entry of a category's `sections` in `data/categories.yaml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionRef {
    /// The title of the part the section is in (`Zones`), when the section's
    /// title alone names several sections.
    pub part: Option<&'static str>,
    /// The section's title (`Double-Faced Cards`).
    pub section: &'static str,
    /// How much of the section.
    pub within: Within,
}

/// The part of a section a [`SectionRef`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Within {
    /// Every rule of the section.
    Whole,
    /// The one rule with this title (`Lifelink`).
    Titled(&'static str),
    /// The rule or sub-rule with this number inside the section (`5`, `5q`):
    /// one that has no title of its own.
    Numbered(&'static str),
}

impl fmt::Display for SectionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(part) = self.part {
            write!(f, "{part} > ")?;
        }
        f.write_str(self.section)?;
        match self.within {
            Within::Whole => Ok(()),
            Within::Titled(rule) => write!(f, " > {rule}"),
            Within::Numbered(number) => write!(f, " > .{number}"),
        }
    }
}

/// A section of the CR: `713. Meld Cards` under `7. Additional Rules`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Section {
    /// The three-digit number (`713`).
    pub id: String,
    /// The title (`Meld Cards`).
    pub title: String,
    /// The title of the part it is in (`Additional Rules`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub part: String,
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.id, self.title)
    }
}

/// Why a reference names nothing in the loaded CR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Missing {
    /// No section has the title (in the part, when one is named).
    NoSection,
    /// Several sections have the title: it needs its part.
    AmbiguousSection,
    /// The section has no rule with that title or number.
    NoRule,
    /// Several rules of the section have the title.
    AmbiguousRule,
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoSection => "no section has this title",
            Self::AmbiguousSection => "several sections have this title; name its part",
            Self::NoRule => "the section has no such rule",
            Self::AmbiguousRule => "several rules of the section have this title",
        })
    }
}

/// A reference of the taxonomy that names nothing in the loaded CR.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Unresolved {
    /// The category's id.
    pub category: String,
    /// The reference as [`SectionRef`] displays it.
    pub reference: String,
    /// Why.
    pub reason: Missing,
}

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.category, self.reference, self.reason)
    }
}

/// What the taxonomy names in one CR.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Each category's rule ids, in the order its references are written:
    /// a section (`713`), a rule (`702.15`) or a sub-rule (`704.5q`).
    pub categories: Vec<(Category, Vec<String>)>,
    /// The references that name nothing, in the taxonomy's order.
    pub unresolved: Vec<Unresolved>,
    /// The sections no category names, whole or in part, in CR order.
    pub uncovered: Vec<Section>,
}

/// The titles of one CR: its sections and its rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outline {
    sections: Vec<Section>,
    /// Rule id (`702.15`) to its title, for rules only (no sub-rules).
    rules: BTreeMap<String, String>,
    /// Every rule and sub-rule id.
    ids: BTreeSet<String>,
}

/// Titles compare without case: the CR's capitalisation of a title has
/// changed where its words have not.
fn same_title(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

impl Outline {
    /// An outline from the CR's sections and its rules: each rule or sub-rule
    /// id with its heading, and whether it is a sub-rule (`702.19b`). A
    /// sub-rule carries its rule's heading, so it never answers to a title.
    #[must_use]
    pub fn new(
        sections: Vec<Section>,
        rules: impl IntoIterator<Item = (String, String, bool)>,
    ) -> Self {
        let mut titled = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for (id, heading, sub_rule) in rules {
            if !sub_rule {
                titled.insert(id.clone(), heading);
            }
            ids.insert(id);
        }
        Self {
            sections,
            rules: titled,
            ids,
        }
    }

    /// Whether the CR it was built from had any section.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// The sections, in CR order.
    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    fn section(&self, r: &SectionRef) -> Result<&Section, Missing> {
        let mut found = self.sections.iter().filter(|s| {
            same_title(&s.title, r.section) && r.part.is_none_or(|p| same_title(&s.part, p))
        });
        let first = found.next().ok_or(Missing::NoSection)?;
        if found.next().is_some() {
            return Err(Missing::AmbiguousSection);
        }
        Ok(first)
    }

    /// The rule id a reference names in this CR.
    ///
    /// # Errors
    /// [`Missing`] when it names nothing, or more than one thing.
    pub fn resolve(&self, r: &SectionRef) -> Result<String, Missing> {
        let section = self.section(r)?;
        match r.within {
            Within::Whole => Ok(section.id.clone()),
            Within::Numbered(number) => {
                let id = format!("{}.{number}", section.id);
                self.ids.contains(&id).then_some(id).ok_or(Missing::NoRule)
            }
            Within::Titled(title) => {
                let prefix = format!("{}.", section.id);
                let mut found = self
                    .rules
                    .range(prefix.clone()..)
                    .take_while(|(id, _)| id.starts_with(&prefix))
                    .filter(|(_, heading)| same_title(heading, title));
                let (id, _) = found.next().ok_or(Missing::NoRule)?;
                if found.next().is_some() {
                    return Err(Missing::AmbiguousRule);
                }
                Ok(id.clone())
            }
        }
    }

    /// Resolve every category's references ([`Category::sections`]).
    #[must_use]
    pub fn resolve_taxonomy(&self) -> Resolution {
        self.resolve_all(Category::ALL.iter().map(|c| (*c, c.sections())))
    }

    /// [`Self::resolve_taxonomy`] over the given references.
    #[must_use]
    pub fn resolve_all<'a>(
        &self,
        taxonomy: impl IntoIterator<Item = (Category, &'a [SectionRef])>,
    ) -> Resolution {
        let mut out = Resolution::default();
        let mut covered = BTreeSet::new();
        for (category, references) in taxonomy {
            let mut ids = Vec::with_capacity(references.len());
            for r in references {
                match self.resolve(r) {
                    Ok(id) => {
                        covered.insert(id.split('.').next().unwrap_or(&id).to_owned());
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    Err(reason) => out.unresolved.push(Unresolved {
                        category: category.id().to_owned(),
                        reference: r.to_string(),
                        reason,
                    }),
                }
            }
            out.categories.push((category, ids));
        }
        out.uncovered = self
            .sections
            .iter()
            .filter(|s| !covered.contains(&s.id))
            .cloned()
            .collect();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(id: &str, title: &str, part: &str) -> Section {
        Section {
            id: id.to_owned(),
            title: title.to_owned(),
            part: part.to_owned(),
        }
    }

    fn rule(id: &str, heading: &str) -> (String, String, bool) {
        let sub_rule = id.ends_with(|c: char| c.is_ascii_lowercase());
        (id.to_owned(), heading.to_owned(), sub_rule)
    }

    /// The CR before a section was inserted at 713.
    fn before() -> Outline {
        Outline::new(
            vec![
                section("100", "General", "Game Concepts"),
                section("400", "General", "Zones"),
                section("702", "Keyword Abilities", "Additional Rules"),
                section("704", "State-Based Actions", "Additional Rules"),
                section("712", "Double-Faced Cards", "Additional Rules"),
                section("714", "Saga Cards", "Additional Rules"),
                section("715", "Adventurer Cards", "Additional Rules"),
                section("903", "Commander", "Casual Variants"),
            ],
            [
                rule("100.1", "General"),
                rule("400.1", "General"),
                rule("702.1", "Keyword Abilities"),
                rule("702.2", "Deathtouch"),
                rule("702.15", "Lifelink"),
                rule("702.150", "Lifelink"),
                rule("704.5", "State-Based Actions"),
                rule("704.5q", "State-Based Actions"),
                rule("712.1", "Double-Faced Cards"),
                rule("714.1", "Saga Cards"),
                rule("715.1", "Adventurer Cards"),
                rule("903.5", "Commander"),
            ],
        )
    }

    /// The same CR with meld cards given a section of their own.
    fn after() -> Outline {
        Outline::new(
            vec![
                section("100", "General", "Game Concepts"),
                section("400", "General", "Zones"),
                section("702", "Keyword Abilities", "Additional Rules"),
                section("704", "State-Based Actions", "Additional Rules"),
                section("712", "Double-Faced Cards", "Additional Rules"),
                section("713", "Meld Cards", "Additional Rules"),
                section("715", "Saga Cards", "Additional Rules"),
                section("716", "Adventurer Cards", "Additional Rules"),
                section("903", "Commander", "Casual Variants"),
            ],
            [
                rule("702.15", "Lifelink"),
                rule("704.5", "State-Based Actions"),
                rule("713.1", "Meld Cards"),
                rule("716.1", "Adventurer Cards"),
            ],
        )
    }

    const fn whole(section: &'static str) -> SectionRef {
        SectionRef {
            part: None,
            section,
            within: Within::Whole,
        }
    }

    const fn within(section: &'static str, within: Within) -> SectionRef {
        SectionRef {
            part: None,
            section,
            within,
        }
    }

    #[test]
    fn a_title_follows_its_section_through_a_renumbering() {
        let adventurer = whole("Adventurer Cards");
        assert_eq!(before().resolve(&adventurer).as_deref(), Ok("715"));
        assert_eq!(after().resolve(&adventurer).as_deref(), Ok("716"));
        // The number it had now belongs to another section, which it never names.
        assert_eq!(after().resolve(&whole("Saga Cards")).as_deref(), Ok("715"));
        assert_eq!(before().resolve(&whole("saga cards")).as_deref(), Ok("714"));
    }

    #[test]
    fn a_missing_title_is_unresolved_never_another_section() {
        assert_eq!(
            before().resolve(&whole("Meld Cards")),
            Err(Missing::NoSection)
        );
        assert_eq!(after().resolve(&whole("Meld Cards")).as_deref(), Ok("713"));
        // Not a prefix or a near match.
        assert_eq!(after().resolve(&whole("Meld")), Err(Missing::NoSection));
    }

    #[test]
    fn a_repeated_title_needs_its_part() {
        assert_eq!(
            before().resolve(&whole("General")),
            Err(Missing::AmbiguousSection)
        );
        let zones = SectionRef {
            part: Some("Zones"),
            ..whole("General")
        };
        assert_eq!(before().resolve(&zones).as_deref(), Ok("400"));
        let nowhere = SectionRef {
            part: Some("Card Types"),
            ..whole("General")
        };
        assert_eq!(before().resolve(&nowhere), Err(Missing::NoSection));
    }

    #[test]
    fn a_rule_is_named_by_its_title_inside_its_section() {
        let o = before();
        let deathtouch = within("Keyword Abilities", Within::Titled("Deathtouch"));
        assert_eq!(o.resolve(&deathtouch).as_deref(), Ok("702.2"));
        // 702.15 and 702.150 are both in range of the `702.` prefix.
        let lifelink = within("Keyword Abilities", Within::Titled("Lifelink"));
        assert_eq!(o.resolve(&lifelink), Err(Missing::AmbiguousRule));
        assert_eq!(after().resolve(&lifelink).as_deref(), Ok("702.15"));
        let flying = within("Keyword Abilities", Within::Titled("Flying"));
        assert_eq!(o.resolve(&flying), Err(Missing::NoRule));
        // A title in another section is not this section's rule.
        let elsewhere = within("Commander", Within::Titled("Lifelink"));
        assert_eq!(o.resolve(&elsewhere), Err(Missing::NoRule));
    }

    #[test]
    fn an_untitled_rule_is_a_number_inside_a_titled_section() {
        let o = before();
        let five = within("Commander", Within::Numbered("5"));
        assert_eq!(o.resolve(&five).as_deref(), Ok("903.5"));
        let sba = within("State-Based Actions", Within::Numbered("5q"));
        assert_eq!(o.resolve(&sba).as_deref(), Ok("704.5q"));
        // The sub-rule is gone from the later CR: reported, not rounded to 704.5.
        assert_eq!(after().resolve(&sba), Err(Missing::NoRule));
        let none = within("Commander", Within::Numbered("99"));
        assert_eq!(o.resolve(&none), Err(Missing::NoRule));
        // A sub-rule never answers to its rule's title.
        let titled = within("State-Based Actions", Within::Titled("State-Based Actions"));
        assert_eq!(o.resolve(&titled).as_deref(), Ok("704.5"));
    }

    #[test]
    fn resolving_reports_what_is_missing_and_what_is_uncovered() {
        let refs = [
            whole("Double-Faced Cards"),
            whole("Meld Cards"),
            whole("Adventurer Cards"),
            whole("Double-Faced Cards"),
            within("Keyword Abilities", Within::Titled("Lifelink")),
        ];
        let taxonomy = || [(Category::MultiFacedCards, refs.as_slice())];

        let old = before().resolve_all(taxonomy());
        assert_eq!(
            old.categories,
            vec![(
                Category::MultiFacedCards,
                vec!["712".to_owned(), "715".to_owned()]
            )]
        );
        let missing: Vec<String> = old.unresolved.iter().map(ToString::to_string).collect();
        assert_eq!(
            missing,
            vec![
                "multi_faced_cards: Meld Cards (no section has this title)",
                "multi_faced_cards: Keyword Abilities > Lifelink (several rules of the section have this title)",
            ]
        );

        let new = after().resolve_all(taxonomy());
        assert_eq!(
            new.categories,
            vec![(
                Category::MultiFacedCards,
                vec![
                    "712".to_owned(),
                    "713".to_owned(),
                    "716".to_owned(),
                    "702.15".to_owned()
                ]
            )]
        );
        assert!(new.unresolved.is_empty());
        // A section named only through one of its rules is covered.
        let uncovered: Vec<String> = new.uncovered.iter().map(ToString::to_string).collect();
        assert_eq!(
            uncovered,
            vec![
                "100 General",
                "400 General",
                "704 State-Based Actions",
                "715 Saga Cards",
                "903 Commander",
            ]
        );
    }

    #[test]
    fn references_display_as_a_path_of_titles() {
        let zones = SectionRef {
            part: Some("Zones"),
            ..whole("General")
        };
        assert_eq!(zones.to_string(), "Zones > General");
        assert_eq!(
            within("Keyword Abilities", Within::Titled("Lifelink")).to_string(),
            "Keyword Abilities > Lifelink"
        );
        assert_eq!(
            within("Commander", Within::Numbered("5")).to_string(),
            "Commander > .5"
        );
    }

    #[test]
    fn every_category_but_other_names_something() {
        for c in Category::ALL {
            assert_eq!(
                c.sections().is_empty(),
                *c == Category::Other,
                "{c}: only `other` has no sections"
            );
        }
    }
}
