//! [`PgResolver`]: the card resolution ladder (ARCHITECTURE.md §3 step 2).
//!
//! A bracketed span (`[[Card Name]]`) is the user naming a card exactly, and
//! takes its own short ladder: exact current or face name → `printed_names`.
//! When neither matches, nothing is resolved; the span is *offered* back as a
//! "did you mean…?": the cards the loose ladder's naming rungs (alias,
//! possessive, short name, alias suffix) point at when there are any — so
//! `[[bolt]]`, typed out of card-fetcher habit, offers Lightning Bolt and is
//! dropped as a duplicate when the extractor also named the card — else the
//! fuzzy neighbours.
//!
//! Anything else takes the full ladder: `card_aliases` → exact current name →
//! `printed_names` → name-before-the-comma → alias suffix → `pg_trgm` fuzzy.
//! Exact rungs match case-insensitively on both `cards.name` and
//! `card_faces.name` (so "Stomp" finds "Bonecrusher Giant // Stomp"); every
//! current name is also a printed name, so `Exact` runs first to keep `via`
//! informative (an errata'd old name reports `PrintedName`).
//! The fuzzy rung scores with `strict_word_similarity`, so a partial name such
//! as "urza" scores 1.0 against every "Urza's …" card and comes back
//! `Ambiguous`, while a short span buried inside a word ("led" in "Grizzled")
//! does not match; the ladder never guesses. When an exact rung matches several
//! cards, one whose *full* name is the span wins over face-name matches
//! ("Lightning Bolt" beats "Emeritus of Conflict // Lightning Bolt").
//! Just before the fuzzy rung, an unbracketed multi-word span is retried
//! against the alias table on each of its word-suffixes ("mirage LED" →
//! `led`), so a set / printing qualifier in front of a nickname does not fuzz
//! onto an unrelated card. The rung is deliberately narrow: it counts only when
//! exactly one suffix is an alias *and* every word before it is a known
//! qualifier ([`QUALIFIERS`]: articles, printing words, classic set names).
//! Otherwise a typo'd real name whose last word happens to be a nickname
//! ("Warleaders Helix" → `helix` → Lightning Helix) would be resolved with
//! confidence instead of reaching fuzzy, which finds the right card
//! (`MatchedVia::AliasSuffix`).

use async_trait::async_trait;
use judge_core::{JudgeError, MatchedVia, Resolution, Resolver};
use nonempty::NonEmpty;
use sqlx::PgPool;
use uuid::Uuid;

use super::{bad_row, cards::load_cards, upstream};

/// Lowest `strict_word_similarity` a fuzzy candidate must reach to be considered.
pub const FUZZY_LOW: f32 = 0.45;
/// A single candidate at or above this similarity is accepted outright.
pub const FUZZY_STRONG: f32 = 0.7;
/// The top candidate is accepted if it leads the runner-up by at least this much.
pub const FUZZY_MARGIN: f32 = 0.15;
/// Most candidates offered in a "did you mean…?".
pub const MAX_CANDIDATES: usize = 5;
/// Fetch one more than offered so the margin rule can see the runner-up.
const FUZZY_FETCH: i64 = 6;

/// `[[Exact Name]]` → exact / printed name only; anything else: alias table →
/// exact name → printed-name table → short name → alias suffix → `pg_trgm` fuzzy.
#[derive(Clone, Debug)]
pub struct PgResolver {
    pool: PgPool,
}

impl PgResolver {
    /// A resolver over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn alias(&self, lowered: &str) -> Result<Option<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            "SELECT oracle_id FROM card_aliases WHERE alias = $1",
            lowered
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(upstream("alias lookup"))
    }

    /// Aliases among `candidates` (already lowercased), with their cards.
    async fn aliases_among(
        &self,
        candidates: &[String],
    ) -> Result<Vec<(String, Uuid)>, JudgeError> {
        let rows = sqlx::query!(
            "SELECT alias, oracle_id FROM card_aliases WHERE alias = ANY($1) ORDER BY alias",
            candidates
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("alias suffix lookup"))?;
        Ok(rows.into_iter().map(|r| (r.alias, r.oracle_id)).collect())
    }

    /// The alias-suffix rung: `None` unless exactly one word-suffix of a
    /// multi-word span is an alias and the words before it are all
    /// [`QUALIFIERS`].
    async fn alias_suffix(
        &self,
        query: &str,
        lowered: &str,
    ) -> Result<Option<Resolution>, JudgeError> {
        let candidates = alias_suffix_candidates(lowered);
        if candidates.is_empty() {
            return Ok(None);
        }
        match self.aliases_among(&candidates).await?.as_slice() {
            [(alias, id)] => {
                if !only_qualifiers_before(lowered, alias) {
                    tracing::debug!(
                        span = query,
                        alias,
                        "alias suffix preceded by a non-qualifier; skipping rung"
                    );
                    return Ok(None);
                }
                tracing::debug!(span = query, alias, "alias suffix matched");
                self.resolved(query, *id, MatchedVia::AliasSuffix)
                    .await
                    .map(Some)
            }
            [] => Ok(None),
            many => {
                tracing::debug!(span = query, aliases = ?many.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(), "several alias suffixes; skipping rung");
                Ok(None)
            }
        }
    }

    /// Case-insensitive exact match on current card names and face names.
    async fn exact_name(&self, lowered: &str) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"
            SELECT oracle_id AS "oracle_id!" FROM cards WHERE lower(name) = $1
            UNION
            SELECT oracle_id FROM card_faces WHERE lower(name) = $1
            "#,
            lowered
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("exact name lookup"))
    }

    async fn printed_name(&self, lowered: &str) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"SELECT DISTINCT oracle_id AS "oracle_id!" FROM printed_names WHERE lower(printed_name) = $1"#,
            lowered
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("printed name lookup"))
    }

    /// Cards (and faces) whose name before the first comma is `lowered`:
    /// "Ragavan" for "Ragavan, Nimble Pilferer" but not "Rashmi and Ragavan".
    async fn short_name(&self, lowered: &str) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"
            SELECT oracle_id AS "oracle_id!" FROM cards
            WHERE position(',' IN name) > 0 AND lower(split_part(name, ',', 1)) = $1
            UNION
            SELECT oracle_id FROM card_faces
            WHERE position(',' IN name) > 0 AND lower(split_part(name, ',', 1)) = $1
            "#,
            lowered
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("short name lookup"))
    }

    /// Among `ids`, those whose full card name is exactly `lowered` (case-insensitive).
    async fn full_name_matches(
        &self,
        lowered: &str,
        ids: &[Uuid],
    ) -> Result<Vec<Uuid>, JudgeError> {
        sqlx::query_scalar!(
            r#"SELECT oracle_id AS "oracle_id!" FROM cards WHERE lower(name) = $1 AND oracle_id = ANY($2)"#,
            lowered,
            ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(upstream("full name lookup"))
    }

    /// Candidates by `strict_word_similarity`, best first, deduplicated per card.
    async fn fuzzy_candidates(&self, text: &str) -> Result<Vec<(Uuid, f32)>, JudgeError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(upstream("begin fuzzy lookup"))?;
        // `<%` filters through the trigram GIN indexes at this (transaction-local) threshold.
        sqlx::query_scalar!(
            "SELECT set_config('pg_trgm.strict_word_similarity_threshold', $1, true)",
            FUZZY_LOW.to_string()
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(upstream("set strict_word_similarity_threshold"))?;
        let rows = sqlx::query!(
            r#"
            WITH cand AS (
                SELECT oracle_id, strict_word_similarity($1, name) AS sim FROM cards WHERE $1 <<% name
                UNION ALL
                SELECT oracle_id, strict_word_similarity($1, name) FROM card_faces WHERE $1 <<% name
            )
            SELECT cand.oracle_id AS "oracle_id!", max(cand.sim)::real AS "sim!"
            FROM cand
            JOIN cards c ON c.oracle_id = cand.oracle_id
            GROUP BY cand.oracle_id, c.name
            HAVING max(cand.sim) >= $2
            ORDER BY max(cand.sim) DESC, length(c.name), c.name
            LIMIT $3
            "#,
            text,
            FUZZY_LOW,
            FUZZY_FETCH
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(upstream("fuzzy lookup"))?;
        tx.commit().await.map_err(upstream("end fuzzy lookup"))?;
        Ok(rows.into_iter().map(|r| (r.oracle_id, r.sim)).collect())
    }

    async fn resolved(
        &self,
        query: &str,
        id: Uuid,
        via: MatchedVia,
    ) -> Result<Resolution, JudgeError> {
        let card = load_cards(&self.pool, &[id])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                bad_row(format!(
                    "card {id} matched {query:?} but has no cards/card_faces row"
                ))
            })?;
        tracing::info!(span = query, card = %card.name, rung = ?via, "card resolved");
        Ok(Resolution::Resolved { card, via })
    }

    /// `None` when `ids` is empty (fall through to the next rung). Several
    /// matches are still unambiguous when exactly one of them is a card whose
    /// full name is the span (the others matched on a face or printed name).
    async fn decide(
        &self,
        query: &str,
        lowered: &str,
        ids: Vec<Uuid>,
        via: MatchedVia,
    ) -> Result<Option<Resolution>, JudgeError> {
        match ids.as_slice() {
            [] => Ok(None),
            [id] => self.resolved(query, *id, via).await.map(Some),
            _ => {
                if let [id] = self.full_name_matches(lowered, &ids).await?.as_slice() {
                    return self.resolved(query, *id, via).await.map(Some);
                }
                self.ambiguous(query, &ids, via).await.map(Some)
            }
        }
    }

    async fn ambiguous(
        &self,
        query: &str,
        ids: &[Uuid],
        rung: MatchedVia,
    ) -> Result<Resolution, JudgeError> {
        let ids: Vec<Uuid> = ids.iter().copied().take(MAX_CANDIDATES).collect();
        let cards = load_cards(&self.pool, &ids).await?;
        tracing::info!(span = query, candidates = cards.len(), rung = ?rung, "card ambiguous");
        Ok(match NonEmpty::from_vec(cards) {
            Some(candidates) => Resolution::Ambiguous {
                query: query.to_owned(),
                candidates,
                via: rung,
            },
            None => Resolution::NotFound {
                query: query.to_owned(),
            },
        })
    }

    /// The bracketed ladder: the exact name, then an old printed name. On a
    /// miss something is offered but never picked, even a single strong
    /// match: the brackets say "this spelling", and a different spelling is
    /// the user's call. The offer is the naming rungs' cards
    /// ([`Self::named_offer`], reported under their own rung so `judge()`
    /// drops the offer when another span resolved one of them), else the fuzzy
    /// neighbours (reported `Fuzzy`, never dropped that way).
    async fn exact_only(&self, query: &str, name: &str) -> Result<Resolution, JudgeError> {
        let lowered = name.to_lowercase();
        let ids = self.exact_name(&lowered).await?;
        if let Some(r) = self
            .decide(query, &lowered, ids, MatchedVia::Bracket)
            .await?
        {
            return Ok(r);
        }
        let ids = self.printed_name(&lowered).await?;
        if let Some(r) = self
            .decide(query, &lowered, ids, MatchedVia::PrintedName)
            .await?
        {
            return Ok(r);
        }
        if let Some((ids, via)) = self.named_offer(&lowered).await? {
            return self.ambiguous(query, &ids, via).await;
        }
        let cands = self.fuzzy_candidates(name).await?;
        if cands.is_empty() {
            tracing::info!(span = query, "bracketed card not found");
            return Ok(Resolution::NotFound {
                query: query.to_owned(),
            });
        }
        let ids: Vec<Uuid> = cands.iter().map(|(id, _)| *id).collect();
        self.ambiguous(query, &ids, MatchedVia::Fuzzy).await
    }

    /// The cards the loose ladder's naming rungs would pick for `lowered`, and
    /// the first rung that named any: alias, the possessive-stripped alias /
    /// exact / short name, short name, alias suffix. Offers only; the fuzzy
    /// rung is not a naming rung and is left to the caller.
    async fn named_offer(
        &self,
        lowered: &str,
    ) -> Result<Option<(Vec<Uuid>, MatchedVia)>, JudgeError> {
        if let Some(id) = self.alias(lowered).await? {
            return Ok(Some((vec![id], MatchedVia::Alias)));
        }
        if let Some(stripped) = strip_possessive(lowered) {
            if let Some(id) = self.alias(&stripped).await? {
                return Ok(Some((vec![id], MatchedVia::Alias)));
            }
            let ids = self.exact_name(&stripped).await?;
            if !ids.is_empty() {
                return Ok(Some((ids, MatchedVia::Exact)));
            }
            let ids = self.short_name(&stripped).await?;
            if !ids.is_empty() {
                return Ok(Some((ids, MatchedVia::ShortName)));
            }
        }
        let ids = self.short_name(lowered).await?;
        if !ids.is_empty() {
            return Ok(Some((ids, MatchedVia::ShortName)));
        }
        let candidates = alias_suffix_candidates(lowered);
        if !candidates.is_empty()
            && let [(alias, id)] = self.aliases_among(&candidates).await?.as_slice()
            && only_qualifiers_before(lowered, alias)
        {
            return Ok(Some((vec![*id], MatchedVia::AliasSuffix)));
        }
        Ok(None)
    }

    /// The full ladder for an unbracketed span.
    async fn ladder(&self, query: &str, text: &str) -> Result<Resolution, JudgeError> {
        let query = query.to_owned();
        let lowered = text.to_lowercase();
        if let Some(id) = self.alias(&lowered).await? {
            return self.resolved(&query, id, MatchedVia::Alias).await;
        }
        // "bob's trigger", "goyf's toughness": retry the alias, exact and short-name
        // rungs on the span with its possessive/punctuation removed.
        if let Some(stripped) = strip_possessive(&lowered) {
            if let Some(id) = self.alias(&stripped).await? {
                return self.resolved(&query, id, MatchedVia::Alias).await;
            }
            let ids = self.exact_name(&stripped).await?;
            if let Some(r) = self
                .decide(&query, &stripped, ids, MatchedVia::Exact)
                .await?
            {
                return Ok(r);
            }
            let ids = self.short_name(&stripped).await?;
            if let Some(r) = self
                .decide(&query, &stripped, ids, MatchedVia::ShortName)
                .await?
            {
                return Ok(r);
            }
        }
        let ids = self.exact_name(&lowered).await?;
        if let Some(r) = self
            .decide(&query, &lowered, ids, MatchedVia::Exact)
            .await?
        {
            return Ok(r);
        }
        let ids = self.printed_name(&lowered).await?;
        if let Some(r) = self
            .decide(&query, &lowered, ids, MatchedVia::PrintedName)
            .await?
        {
            return Ok(r);
        }
        let ids = self.short_name(&lowered).await?;
        if let Some(r) = self
            .decide(&query, &lowered, ids, MatchedVia::ShortName)
            .await?
        {
            return Ok(r);
        }
        if let Some(r) = self.alias_suffix(&query, &lowered).await? {
            return Ok(r);
        }
        self.fuzzy(&query, text).await
    }

    async fn fuzzy(&self, query: &str, text: &str) -> Result<Resolution, JudgeError> {
        let cands = self.fuzzy_candidates(text).await?;
        let winner = match cands.as_slice() {
            [] => None,
            [(id, _)] => Some(*id),
            [(id, top), (_, second), ..] => {
                let strong = cands.iter().filter(|(_, s)| *s >= FUZZY_STRONG).count();
                ((strong == 1 && *top >= FUZZY_STRONG) || top - second >= FUZZY_MARGIN)
                    .then_some(*id)
            }
        };
        if let Some(id) = winner {
            return self.resolved(query, id, MatchedVia::Fuzzy).await;
        }
        if cands.is_empty() {
            tracing::info!(span = query, "card not found");
            return Ok(Resolution::NotFound {
                query: query.to_owned(),
            });
        }
        let ids: Vec<Uuid> = cands.iter().map(|(id, _)| *id).collect();
        self.ambiguous(query, &ids, MatchedVia::Fuzzy).await
    }
}

/// Words allowed in front of a nickname for the alias-suffix rung: articles
/// and possessives, printing / finish qualifiers, and classic set names. Not
/// exhaustive on purpose — an unknown word means "this is probably a card
/// name", and fuzzy gets the span instead.
pub const QUALIFIERS: &[&str] = &[
    // articles, possessives, filler
    "a",
    "an",
    "the",
    "my",
    "your",
    "his",
    "her",
    "their",
    "our",
    "this",
    "that",
    "of",
    // printings and finishes
    "foil",
    "nonfoil",
    "promo",
    "borderless",
    "showcase",
    "extended",
    "retro",
    "etched",
    "textless",
    "old",
    "new",
    "frame",
    "judge",
    "fnm",
    "prerelease",
    "misprint",
    "altered",
    "signed",
    "proxy",
    "card",
    "copy",
    "version",
    "printing",
    "printed",
    "edition",
    "original",
    "reprint",
    "reprinted",
    "reserved",
    "list",
    "secret",
    "lair",
    "mystical",
    "archive",
    "masters",
    "modern",
    "eternal",
    "vintage",
    "legacy",
    "commander",
    "collectors",
    // classic set names (single words only)
    "alpha",
    "beta",
    "unlimited",
    "revised",
    "arabian",
    "nights",
    "antiquities",
    "legends",
    "dark",
    "fallen",
    "empires",
    "ice",
    "age",
    "homelands",
    "alliances",
    "mirage",
    "visions",
    "weatherlight",
    "tempest",
    "stronghold",
    "exodus",
    "urza's",
    "saga",
    "destiny",
    "mercadian",
    "masques",
    "nemesis",
    "prophecy",
    "invasion",
    "planeshift",
    "apocalypse",
    "odyssey",
    "torment",
    "judgment",
    "onslaught",
    "mirrodin",
    "kamigawa",
    "ravnica",
    "chronicles",
    "portal",
    "starter",
    "conspiracy",
    "jumpstart",
    "horizons",
];

/// For a multi-word span, every proper word-suffix ("b c", "c" of "a b c"),
/// deduplicated; empty for a single word (the whole span was already tried
/// against the alias table).
fn alias_suffix_candidates(lowered: &str) -> Vec<String> {
    let words: Vec<&str> = lowered.split_whitespace().collect();
    if words.len() < 2 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for i in 1..words.len() {
        let suffix = words.get(i..).unwrap_or_default().join(" ");
        if !out.contains(&suffix) {
            out.push(suffix);
        }
    }
    out
}

/// True if every word of `lowered` before the trailing `alias` is a
/// [`QUALIFIERS`] entry. False if `alias` is not a word-suffix of `lowered`
/// or nothing precedes it.
fn only_qualifiers_before(lowered: &str, alias: &str) -> bool {
    let words: Vec<&str> = lowered.split_whitespace().collect();
    let alias_words: Vec<&str> = alias.split_whitespace().collect();
    let Some(prefix_len) = words.len().checked_sub(alias_words.len()) else {
        return false;
    };
    if prefix_len == 0 || words.get(prefix_len..) != Some(alias_words.as_slice()) {
        return false;
    }
    words
        .get(..prefix_len)
        .is_some_and(|prefix| prefix.iter().all(|w| QUALIFIERS.contains(w)))
}

/// Drop a trailing English possessive (`bob's`, `goyf’s`, `Jace's`) and
/// trailing punctuation so nicknames written in running prose still reach
/// the alias and name rungs. Returns `None` when nothing was stripped.
fn strip_possessive(lowered: &str) -> Option<String> {
    let t = lowered.trim_end_matches(['?', '!', '.', ',', ':', ';']);
    let t = t
        .strip_suffix("'s")
        .or_else(|| t.strip_suffix("\u{2019}s"))
        .unwrap_or(t)
        .trim_end();
    (!t.is_empty() && t != lowered).then(|| t.to_owned())
}

/// How a span asks to be matched. The two ladders are separate methods
/// selected by an exhaustive match, so no rung of the loose ladder (alias,
/// short name, alias suffix, a confident fuzzy pick) can reach a bracketed span.
#[derive(Debug, PartialEq, Eq)]
enum CardSpan<'a> {
    /// `[[Card Name]]`: the inner name, trimmed.
    Exact(&'a str),
    /// Anything else, as written.
    Loose(&'a str),
}

impl<'a> CardSpan<'a> {
    fn parse(s: &'a str) -> Self {
        s.strip_prefix("[[")
            .and_then(|inner| inner.strip_suffix("]]"))
            .map_or(Self::Loose(s), |inner| Self::Exact(inner.trim()))
    }
}

#[async_trait]
impl Resolver for PgResolver {
    async fn resolve(&self, span: &str) -> Result<Resolution, JudgeError> {
        let query = span.trim();
        match CardSpan::parse(query) {
            CardSpan::Exact(text) | CardSpan::Loose(text) if text.is_empty() => {
                Ok(Resolution::NotFound {
                    query: query.to_owned(),
                })
            }
            CardSpan::Exact(name) => self.exact_only(query, name).await,
            CardSpan::Loose(text) => self.ladder(query, text).await,
        }
    }
}

#[cfg(test)]
mod unit {
    use super::{CardSpan, alias_suffix_candidates, only_qualifiers_before, strip_possessive};

    #[test]
    fn suffix_candidates() {
        assert!(alias_suffix_candidates("led").is_empty());
        assert_eq!(alias_suffix_candidates("mirage led"), ["led"]);
        assert_eq!(
            alias_suffix_candidates("the foil  bob"),
            ["foil bob", "bob"]
        );
        assert_eq!(alias_suffix_candidates("a b a"), ["b a", "a"]);
    }

    #[test]
    fn qualifier_gate() {
        assert!(only_qualifiers_before("mirage led", "led"));
        assert!(only_qualifiers_before("the foil bob", "bob"));
        assert!(only_qualifiers_before("the foil bob", "foil bob"));
        assert!(!only_qualifiers_before("warleaders helix", "helix"));
        assert!(
            !only_qualifiers_before("lattice blade mantis", "lattice"),
            "not a suffix"
        );
        assert!(!only_qualifiers_before("bob led", "led"));
        assert!(!only_qualifiers_before("led", "led"), "nothing precedes it");
        assert!(!only_qualifiers_before("foil", "foil bob"));
    }

    #[test]
    fn brackets() {
        assert_eq!(
            CardSpan::parse("[[ Dark Confidant ]]"),
            CardSpan::Exact("Dark Confidant")
        );
        assert_eq!(
            CardSpan::parse("Dark Confidant"),
            CardSpan::Loose("Dark Confidant")
        );
        assert_eq!(CardSpan::parse("[[oops"), CardSpan::Loose("[[oops"));
        assert_eq!(CardSpan::parse("[[ ]]"), CardSpan::Exact(""));
    }

    #[test]
    fn strip_possessive_handles_ascii_and_curly_apostrophes() {
        assert_eq!(strip_possessive("bob's").as_deref(), Some("bob"));
        assert_eq!(strip_possessive("goyf\u{2019}s?").as_deref(), Some("goyf"));
        assert_eq!(strip_possessive("tibalt's,").as_deref(), Some("tibalt"));
        assert_eq!(strip_possessive("dark confidant"), None);
        assert_eq!(strip_possessive("'s"), None);
    }
}

#[cfg(test)]
mod pg {
    //! The alias-suffix rung against a throwaway database (`DATABASE_URL`
    //! via dotenvy; `#[sqlx::test]` applies `./migrations`).

    use judge_core::{MatchedVia, Resolution, Resolver};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::PgResolver;

    const LED: Uuid = Uuid::from_u128(101);
    const BOB: Uuid = Uuid::from_u128(102);
    const GRIZZLED: Uuid = Uuid::from_u128(103);
    const LIGHTNING_HELIX: Uuid = Uuid::from_u128(104);
    const WARLEADERS_HELIX: Uuid = Uuid::from_u128(105);
    const BAZAAR: Uuid = Uuid::from_u128(106);

    async fn seed(pool: &PgPool) -> anyhow::Result<()> {
        for (id, name, text) in [
            (
                LED,
                "Lion's Eye Diamond",
                "Sacrifice this artifact, Discard your hand: Add three mana of any one color.",
            ),
            (
                BOB,
                "Dark Confidant",
                "At the beginning of your upkeep, reveal the top card of your library.",
            ),
            (GRIZZLED, "Grizzled Leotau", ""),
            (
                LIGHTNING_HELIX,
                "Lightning Helix",
                "Lightning Helix deals 3 damage to any target and you gain 3 life.",
            ),
            (
                WARLEADERS_HELIX,
                "Warleader's Helix",
                "Warleader's Helix deals 4 damage to any target and you gain 4 life.",
            ),
            (
                BAZAAR,
                "Bazaar of Baghdad",
                "{T}: Draw two cards, then discard three cards.",
            ),
        ] {
            sqlx::query("INSERT INTO cards (oracle_id, name, layout) VALUES ($1, $2, 'normal')")
                .bind(id)
                .bind(name)
                .execute(pool)
                .await?;
            sqlx::query("INSERT INTO card_faces (oracle_id, face_idx, name, oracle_text) VALUES ($1, 0, $2, $3)")
                .bind(id).bind(name).bind(text).execute(pool).await?;
        }
        sqlx::query("INSERT INTO card_aliases (alias, oracle_id) VALUES ('led', $1), ('bob', $2), ('confidant', $2), ('helix', $3), ('bazaar', $4)")
            .bind(LED).bind(BOB).bind(LIGHTNING_HELIX).bind(BAZAAR).execute(pool).await?;
        Ok(())
    }

    fn resolved(r: &Resolution) -> Option<(&str, MatchedVia)> {
        match r {
            Resolution::Resolved { card, via } => Some((card.name.as_str(), *via)),
            _ => None,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn qualifier_before_a_nickname_resolves_via_alias_suffix(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        seed(&pool).await?;
        let resolver = PgResolver::new(pool);
        assert_eq!(
            resolved(&resolver.resolve("mirage LED").await?),
            Some(("Lion's Eye Diamond", MatchedVia::AliasSuffix))
        );
        assert_eq!(
            resolved(&resolver.resolve("the foil Bob").await?),
            Some(("Dark Confidant", MatchedVia::AliasSuffix))
        );
        // A plain alias still reports `Alias`; a single non-alias word never reaches the rung.
        assert_eq!(
            resolved(&resolver.resolve("LED").await?),
            Some(("Lion's Eye Diamond", MatchedVia::Alias))
        );
        assert!(matches!(
            resolver.resolve("mirage").await?,
            Resolution::NotFound { .. }
        ));
        // Two different aliases in one span: the rung stands down and the ladder falls through.
        let r = resolver.resolve("bob led").await?;
        assert!(
            !matches!(
                &r,
                Resolution::Resolved {
                    via: MatchedVia::AliasSuffix,
                    ..
                }
            ),
            "{r:?}"
        );
        // Exact rungs still win over the suffix rung.
        assert_eq!(
            resolved(&resolver.resolve("dark confidant").await?),
            Some(("Dark Confidant", MatchedVia::Exact))
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn alias_suffix_does_not_fire_on_typoed_names_or_brackets(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        seed(&pool).await?;
        let resolver = PgResolver::new(pool);
        // A dropped apostrophe: "warleaders" is no qualifier, so the rung stands down and fuzzy finds the real card.
        let r = resolver.resolve("Warleaders Helix").await?;
        assert!(
            !matches!(
                &r,
                Resolution::Resolved {
                    via: MatchedVia::AliasSuffix,
                    ..
                }
            ),
            "{r:?}"
        );
        assert_eq!(resolved(&r), Some(("Warleader's Helix", MatchedVia::Fuzzy)));
        // The alias is not the suffix ("bazaar traders"): nothing to match.
        let r = resolver.resolve("bazaar traders").await?;
        assert!(
            !matches!(
                &r,
                Resolution::Resolved {
                    via: MatchedVia::AliasSuffix,
                    ..
                }
            ),
            "{r:?}"
        );
        // A bracketed span is exact spelling; the rung is skipped even with a qualifier prefix.
        let r = resolver.resolve("[[foil helix]]").await?;
        assert!(!matches!(&r, Resolution::Resolved { .. }), "{r:?}");
        // The intended case still works with a qualifier prefix.
        assert_eq!(
            resolved(&resolver.resolve("foil helix").await?),
            Some(("Lightning Helix", MatchedVia::AliasSuffix))
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn brackets_match_exact_names_only(pool: PgPool) -> anyhow::Result<()> {
        seed(&pool).await?;
        let resolver = PgResolver::new(pool);
        assert_eq!(
            resolved(&resolver.resolve("[[ dark confidant ]]").await?),
            Some(("Dark Confidant", MatchedVia::Bracket))
        );
        // A nickname in brackets is not resolved, only offered, under the rung
        // that named it (so another span resolving the card drops the offer).
        let offered = |r: Resolution| match r {
            Resolution::Ambiguous {
                candidates, via, ..
            } => Some((
                candidates
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>(),
                via,
            )),
            _ => None,
        };
        assert_eq!(
            offered(resolver.resolve("[[Bob]]").await?),
            Some((vec!["Dark Confidant".to_owned()], MatchedVia::Alias))
        );
        assert_eq!(
            offered(resolver.resolve("[[bob's]]").await?),
            Some((vec!["Dark Confidant".to_owned()], MatchedVia::Alias))
        );
        assert_eq!(
            offered(resolver.resolve("[[mirage LED]]").await?),
            Some((
                vec!["Lion's Eye Diamond".to_owned()],
                MatchedVia::AliasSuffix
            ))
        );
        // A typo that fuzzy would resolve on its own is offered, not picked.
        assert_eq!(
            resolved(&resolver.resolve("Warleaders Helix").await?),
            Some(("Warleader's Helix", MatchedVia::Fuzzy))
        );
        match resolver.resolve("[[Warleaders Helix]]").await? {
            Resolution::Ambiguous {
                query,
                candidates,
                via,
            } => {
                assert_eq!(query, "[[Warleaders Helix]]");
                assert_eq!(via, MatchedVia::Fuzzy);
                assert_eq!(candidates.head.name, "Warleader's Helix");
            }
            other => anyhow::bail!("expected an offer, got {other:?}"),
        }
        assert!(matches!(
            resolver.resolve("[[Nothing Like Any Card]]").await?,
            Resolution::NotFound { .. }
        ));
        assert!(matches!(
            resolver.resolve("[[  ]]").await?,
            Resolution::NotFound { .. }
        ));
        Ok(())
    }
}
