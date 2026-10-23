//! Invented cards for the unreleased-card questions (D42).
//!
//! A real previewed card stops being unreleased on its release day, so a gold
//! question about one goes stale. A fixture card ([`FixtureCard`]) exists only
//! in the gold file and is first printed a fixed number of days after the run,
//! so it is always upcoming, and no model has seen it. Nothing is written to
//! the database: [`FixtureResolver`] answers the fixtures' names and hands
//! every other span to the real resolver, and each fixture's [`Release`] is
//! judged against the loaded CR by `judge_bot::db::assess_release`, the check a
//! stored card gets. Retrieval finds no rulings, notes or prior calls for a
//! fixture's id, which is what an unreleased card has.

use std::sync::Arc;

use async_trait::async_trait;
use judge_core::{Card, CardId, Face, JudgeError, Layout, MatchedVia, Resolution, Resolver};
use nonempty::NonEmpty;
use sqlx::PgPool;

use crate::gold::{FixtureCard, GoldQuestion};

/// Every fixture card of a gold file, built for this run.
#[derive(Clone, Debug, Default)]
pub struct Fixtures {
    cards: Vec<Card>,
}

impl Fixtures {
    /// Build the fixtures of `questions`, refusing one whose name `real`
    /// resolves as a bracketed name (exact, printed name or alias, never
    /// fuzzy): a fixture must be invented, or the question would test the
    /// real card's data instead.
    ///
    /// # Errors
    /// A fixture that names a real card, or a database failure.
    pub async fn build<'a>(
        pool: &PgPool,
        real: &dyn Resolver,
        questions: impl IntoIterator<Item = &'a GoldQuestion>,
    ) -> anyhow::Result<Self> {
        let mut cards: Vec<Card> = Vec::new();
        for f in questions.into_iter().flat_map(|q| &q.fixture_cards) {
            if let Some(seen) = cards.iter().find(|c| c.name.eq_ignore_ascii_case(&f.name)) {
                // One name is one card: a second definition must say the same.
                let face = seen.faces.first();
                anyhow::ensure!(
                    face.oracle_text == f.oracle_text
                        && face.type_line == f.type_line
                        && face.mana_cost == f.mana_cost,
                    "fixture card {:?} is defined twice with different text",
                    f.name
                );
                continue;
            }
            let bracketed = format!("[[{}]]", f.name);
            if let Resolution::Resolved { card, via } = real.resolve(&bracketed).await? {
                anyhow::bail!(
                    "fixture card {:?} resolves to the real card {:?} ({via:?}): rename it",
                    f.name,
                    card.name
                );
            }
            cards.push(card(pool, f).await?);
        }
        Ok(Self { cards })
    }

    /// The fixture `span` names (brackets, case, surrounding space and a
    /// possessive aside: "Quorbit Adept's" must not go on to fuzzy-match a
    /// real card).
    fn find(&self, span: &str) -> Option<&Card> {
        let name = span
            .trim()
            .trim_start_matches("[[")
            .trim_end_matches("]]")
            .trim();
        let bare = name
            .strip_suffix("'s")
            .or_else(|| name.strip_suffix("’s"))
            .unwrap_or(name);
        self.cards
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name) || c.name.eq_ignore_ascii_case(bare))
    }

    /// `real`, answering the fixtures' names first.
    #[must_use]
    pub fn resolver(&self, real: Arc<dyn Resolver>) -> Arc<dyn Resolver> {
        if self.cards.is_empty() {
            return real;
        }
        Arc::new(FixtureResolver {
            fixtures: self.clone(),
            real,
        })
    }
}

async fn card(pool: &PgPool, f: &FixtureCard) -> anyhow::Result<Card> {
    let face = Face {
        name: f.name.clone(),
        oracle_text: f.oracle_text.clone(),
        mana_cost: f.mana_cost.clone(),
        type_line: f.type_line.clone(),
    };
    let release = judge_bot::db::assess_release(
        pool,
        std::slice::from_ref(&face),
        &f.keywords,
        f.first_printed_in_days,
    )
    .await?;
    Ok(Card {
        id: CardId::new(uuid::Uuid::new_v4()),
        name: f.name.clone(),
        layout: Layout::Normal,
        faces: NonEmpty::new(face),
        release,
    })
}

/// A resolver that knows the fixtures' names, and asks the real one for the rest.
struct FixtureResolver {
    fixtures: Fixtures,
    real: Arc<dyn Resolver>,
}

#[async_trait]
impl Resolver for FixtureResolver {
    async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError> {
        match self.fixtures.find(span) {
            Some(card) => Ok(Resolution::Resolved {
                card: card.clone(),
                via: MatchedVia::Exact,
            }),
            None => self.real.resolve(span).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Fixtures {
        Fixtures {
            cards: vec![Card {
                id: CardId::new(uuid::Uuid::from_u128(1)),
                name: "Quorbit Adept".into(),
                layout: Layout::Normal,
                faces: NonEmpty::new(Face {
                    name: "Quorbit Adept".into(),
                    oracle_text: "Whenever you quorbit, draw a card.".into(),
                    mana_cost: String::new(),
                    type_line: "Creature — Human Wizard".into(),
                }),
                release: judge_core::Release::Released,
            }],
        }
    }

    #[test]
    fn a_span_finds_its_fixture_however_it_is_written() {
        let f = fixtures();
        for span in [
            "Quorbit Adept",
            "[[Quorbit Adept]]",
            " quorbit adept ",
            "Quorbit Adept's",
            "[[Quorbit Adept’s]]",
        ] {
            assert!(f.find(span).is_some(), "{span}");
        }
        assert!(f.find("Quorbit").is_none());
    }
}
