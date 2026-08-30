//! Domain types (ARCHITECTURE.md §5). Newtypes carry their invariants;
//! enums are exhaustive; nothing here is nullable.

use nonempty::NonEmpty;
use nutype::nutype;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use regex::Regex;
use std::{borrow::Cow, fmt, sync::LazyLock};
use uuid::Uuid;

use crate::Category;

/// Regex for `RuleId`: `702` | `702.19` | `702.19b` | `704.5aa` (the CR runs
/// past `z` into two-letter sub-rules). ASCII digits only: the `regex` crate's
/// `\d` would accept any Unicode `Nd` digit, which can never match a
/// `rules.id` row. Shared by the nutype validator below.
pub const RULE_ID_PATTERN: &str = r"^[0-9]{3}(\.[0-9]+[a-z]{0,2})?$";
/// Regex for `CrVersion`: the CR effective date as `YYYYMMDD` (ASCII digits).
pub const CR_VERSION_PATTERN: &str = r"^[0-9]{8}$";

static RULE_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(RULE_ID_PATTERN).expect("RULE_ID_PATTERN is a valid regex")
});
static CR_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(CR_VERSION_PATTERN).expect("CR_VERSION_PATTERN is a valid regex")
});

/// A Comprehensive Rules identifier such as `702.19`, `702.19b` or `704.5aa`.
#[nutype(
    sanitize(trim),
    validate(regex = RULE_ID_RE),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, Hash, AsRef)
)]
pub struct RuleId(String);

impl JsonSchema for RuleId {
    fn schema_name() -> Cow<'static, str> {
        "RuleId".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            // No "pattern": Anthropic's structured-output subset rejects it. nutype
            // still enforces RULE_ID_PATTERN when the response is deserialized.
            "description": "Comprehensive Rules id such as 702.19 or 702.19b (three digits, optional .number and one or two letters)"
        })
    }
}

/// Scryfall `oracle_id`.
#[nutype(derive(Clone, Copy, Debug, Display, Serialize, Deserialize, PartialEq, Eq, Hash, AsRef))]
pub struct CardId(Uuid);

impl JsonSchema for CardId {
    fn schema_name() -> Cow<'static, str> {
        "CardId".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "format": "uuid", "description": "Scryfall oracle_id" })
    }
}

/// Identifier of a persisted call (question + verdict).
#[nutype(derive(Clone, Copy, Debug, Display, Serialize, Deserialize, PartialEq, Eq, Hash, AsRef))]
pub struct CallId(Uuid);

impl JsonSchema for CallId {
    fn schema_name() -> Cow<'static, str> {
        "CallId".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "format": "uuid", "description": "Prior call id" })
    }
}

/// Comprehensive Rules version, as its effective date `YYYYMMDD`.
#[nutype(
    sanitize(trim),
    validate(regex = CR_VERSION_RE),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, Hash, AsRef)
)]
pub struct CrVersion(String);

impl JsonSchema for CrVersion {
    fn schema_name() -> Cow<'static, str> {
        "CrVersion".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        // No "pattern" (unsupported by Anthropic); nutype validates on deserialize.
        json_schema!({ "type": "string", "description": "CR effective date as eight digits, YYYYMMDD" })
    }
}

/// Scryfall card layout (variant names mirror Scryfall's `layout` values).
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Normal,
    Split,
    Flip,
    Transform,
    ModalDfc,
    Meld,
    Leveler,
    Class,
    Case,
    Saga,
    Adventure,
    Mutate,
    Prototype,
    Prepare,
    Battle,
    Planar,
    Scheme,
    Vanguard,
    Token,
    DoubleFacedToken,
    Emblem,
    Augment,
    Host,
    ArtSeries,
    ReversibleCard,
}

/// One face of a card. Single-faced cards have exactly one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Face {
    /// Face name.
    pub name: String,
    /// Current Oracle text.
    pub oracle_text: String,
    /// Mana cost in Scryfall notation, e.g. `{1}{B}`.
    pub mana_cost: String,
    /// Full type line.
    pub type_line: String,
}

/// A card as identified by its oracle id, with all faces.
/// (No `JsonSchema`: cards are never part of a model-facing schema, and
/// `nonempty` has no schemars 1.x support.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    /// Scryfall oracle id.
    pub id: CardId,
    /// Full card name.
    pub name: String,
    /// Scryfall layout.
    pub layout: Layout,
    /// All faces; single-faced cards have exactly one.
    pub faces: NonEmpty<Face>,
}

/// Which rules body a question falls under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Comprehensive Rules.
    Cr,
    /// Commander format rules (CR 903 + Commander Rules Committee).
    Commander,
    /// Tournament policy (MTR/IPG). Not covered by the prototype.
    Tournament,
    /// Not a Magic rules question.
    OutOfScope,
}

impl Source {
    /// Whether the bot answers questions from this source at all.
    #[must_use]
    pub const fn is_answerable(self) -> bool {
        self.answerable().is_some()
    }

    /// The answerable subset, or `None` for `Tournament` / `OutOfScope`. A
    /// `Verdict` can only be validated against an [`AnswerableSource`], so the
    /// "not answerable" branch has to be taken before synthesis is reached.
    #[must_use]
    pub const fn answerable(self) -> Option<AnswerableSource> {
        match self {
            Source::Cr => Some(AnswerableSource::Cr),
            Source::Commander => Some(AnswerableSource::Commander),
            Source::Tournament | Source::OutOfScope => None,
        }
    }
}

/// The rules bodies the bot answers from: [`Source`] minus the variants
/// `judge()` refuses. Stamped onto a verdict from the *extraction* (the model
/// never reports it at synthesis time), so a verdict's source cannot disagree
/// with the classification and every verdict must cite something.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerableSource {
    /// Comprehensive Rules.
    Cr,
    /// Commander format rules (CR 903 + Commander Rules Committee).
    Commander,
}

impl From<AnswerableSource> for Source {
    fn from(s: AnswerableSource) -> Self {
        match s {
            AnswerableSource::Cr => Source::Cr,
            AnswerableSource::Commander => Source::Commander,
        }
    }
}

/// Model self-reported confidence in a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Model is unsure; rendering should say so.
    Low,
    /// Reasonably sure.
    Medium,
    /// Sure.
    High,
}

/// How a card-name span was matched to a card (resolution ladder order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchedVia {
    /// Hand-curated nickname table.
    Alias,
    /// `[[Card Name]]` syntax.
    Bracket,
    /// Exact current name.
    Exact,
    /// Old / errata'd printed name.
    PrintedName,
    /// The part of a name before its first comma (`Ragavan` for
    /// "Ragavan, Nimble Pilferer"): how legendary cards are usually referred to.
    ShortName,
    /// `pg_trgm` similarity.
    Fuzzy,
}

/// An ambiguous span with its candidate cards ("did you mean…?").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ambiguous {
    /// The span as the user wrote it.
    pub query: String,
    /// Cards it could refer to.
    pub candidates: NonEmpty<Card>,
}

/// Outcome of resolving one card-name span. Never guesses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// Exactly one card matched.
    Resolved {
        /// The card.
        card: Card,
        /// Which rung of the ladder matched.
        via: MatchedVia,
    },
    /// Several cards matched; ask the user.
    Ambiguous {
        /// The span as written.
        query: String,
        /// Cards it could be.
        candidates: NonEmpty<Card>,
        /// Which rung produced the candidates. `Fuzzy` candidates are trigram
        /// neighbours, not names the user could have meant, so `judge()` never
        /// treats a fuzzy-ambiguous span as a duplicate of a resolved card.
        via: MatchedVia,
    },
    /// Nothing matched.
    NotFound {
        /// The span as written.
        query: String,
    },
}

/// A typed reference plus the exact span quoted from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Citation {
    /// A Comprehensive Rules chunk.
    Rule {
        /// Rule id.
        id: RuleId,
        /// Verbatim span of the rule.
        quote: String,
    },
    /// Ruling `idx` of `card` on Scryfall.
    ScryfallRuling {
        /// The card.
        card: CardId,
        /// Ruling index for that card.
        idx: u32,
        /// Verbatim span of the ruling.
        quote: String,
    },
    /// A prior rated call (example, never an authority).
    PriorCall {
        /// Call id.
        id: CallId,
        /// Verbatim span of the call's answer.
        quote: String,
    },
}

impl Citation {
    /// The quoted span, whatever the reference kind.
    #[must_use]
    pub fn quote(&self) -> &str {
        match self {
            Citation::Rule { quote, .. }
            | Citation::ScryfallRuling { quote, .. }
            | Citation::PriorCall { quote, .. } => quote,
        }
    }
}

impl fmt::Display for Citation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Citation::Rule { id, quote } => write!(f, "rule {id}: {quote:?}"),
            Citation::ScryfallRuling { card, idx, quote } => write!(f, "ruling {card}#{idx}: {quote:?}"),
            Citation::PriorCall { id, quote } => write!(f, "prior call {id}: {quote:?}"),
        }
    }
}

/// A CR chunk at rule granularity (e.g. `702.19` with its lettered sub-rules).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleChunk {
    /// Rule id, e.g. `702.19`.
    pub id: RuleId,
    /// Enclosing rule, if this is a sub-rule.
    pub parent_id: Option<RuleId>,
    /// Three-digit subsection this chunk belongs to, e.g. `702`.
    pub subsection: RuleId,
    /// Rule heading, e.g. `Lifelink`.
    pub heading: String,
    /// Rule text including lettered sub-rules.
    pub body: String,
    /// `Example:` lines belonging to this rule.
    pub examples: Vec<String>,
    /// CR release this chunk was parsed from.
    pub cr_version: CrVersion,
}

impl RuleChunk {
    /// True if `quote` appears verbatim in the body or any example.
    #[must_use]
    pub fn contains_quote(&self, quote: &str) -> bool {
        self.body.contains(quote) || self.examples.iter().any(|e| e.contains(quote))
    }
}

/// A Scryfall ruling for a card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    /// The card the ruling is about.
    pub card: CardId,
    /// Position in Scryfall's ruling list for the card.
    pub idx: u32,
    /// ISO-8601 date as published by Scryfall.
    pub published_at: String,
    /// Ruling text.
    pub text: String,
}

/// A CR glossary entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlossaryEntry {
    /// Glossary headword.
    pub term: String,
    /// Definition.
    pub text: String,
}

/// A prior rated call, shown to the model as an *example* after CR material.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PriorCall {
    /// Call id.
    pub id: CallId,
    /// The question as asked.
    pub question: String,
    /// The answer given.
    pub answer: String,
    /// Category assigned at the time.
    pub category: Category,
    /// Citations of the answer.
    pub citations: Vec<Citation>,
    /// CR version the call was made under.
    pub cr_version: CrVersion,
    /// Bayesian-smoothed mean rating in `1.0..=3.0`.
    pub rating: f32,
    /// Number of ratings received.
    pub rating_count: u32,
}

/// Hand-written note for a "nightmare" card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardNote {
    /// The card the note is about.
    pub card: CardId,
    /// Markdown note.
    pub note: String,
}

/// One previous question/answer pair in the same thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Qa {
    /// Earlier question.
    pub question: String,
    /// Its answer.
    pub answer: String,
}

/// An incoming question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Discord thread (or channel) id the question arrived in.
    pub thread_id: String,
    /// Message text.
    pub text: String,
}

/// A category guess from the classifier.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CategoryGuess {
    /// Guessed category.
    pub category: Category,
    /// Classifier confidence.
    pub confidence: Confidence,
}

/// Output of the extraction + classification LLM call (pipeline steps 1 and 3).
///
/// The best category is a required scalar (`primary`), so the model-facing
/// schema *requires* one: an answer with no category is rejected by the API
/// itself rather than patched up after the fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Extraction {
    /// Candidate card-name spans exactly as written by the user.
    pub card_spans: Vec<String>,
    /// Rules concepts / keywords the fuzzy retrieval legs should see.
    pub concepts: Vec<String>,
    /// The single best-fitting category. Required.
    pub primary: CategoryGuess,
    /// Up to two further categories, best first. Extras beyond two, and any
    /// repeat of `primary`, are ignored by [`Extraction::categories`].
    #[serde(default)]
    pub secondary: Vec<CategoryGuess>,
    /// Which rules body the question falls under.
    pub source: Source,
}

impl Extraction {
    /// Maximum number of secondary categories honoured.
    pub const MAX_SECONDARY: usize = 2;

    /// The best category.
    #[must_use]
    pub fn primary_category(&self) -> Category {
        self.primary.category
    }

    /// `primary` first, then the secondary guesses with duplicates (of the
    /// primary or of an earlier secondary) removed and at most
    /// [`Self::MAX_SECONDARY`] of them kept.
    pub fn categories(&self) -> impl Iterator<Item = &CategoryGuess> {
        let mut seen = vec![self.primary.category];
        std::iter::once(&self.primary).chain(
            self.secondary
                .iter()
                .filter(move |g| {
                    if seen.contains(&g.category) {
                        false
                    } else {
                        seen.push(g.category);
                        true
                    }
                })
                .take(Self::MAX_SECONDARY),
        )
    }
}

/// Why a verdict was rejected for being empty rather than for a bad citation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyVerdict {
    /// The answer cites nothing (every verdict draws on the CR / Commander rules).
    NoCitations,
    /// The answer text is shorter than [`crate::verdict::MIN_ANSWER_CHARS`].
    ShortAnswer {
        /// Characters in the trimmed answer.
        chars: usize,
    },
}

impl fmt::Display for EmptyVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmptyVerdict::NoCitations => write!(f, "the answer had no citations"),
            EmptyVerdict::ShortAnswer { chars } => write!(f, "the answer was empty or too short ({chars} characters)"),
        }
    }
}

/// What a previous synthesis attempt was rejected for; the synthesizer shows
/// this to the model on the retry. Exhaustive, so a new rejection reason must
/// be rendered before it compiles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rejection {
    /// A citation failed validation.
    BadCitation(Citation),
    /// The verdict was empty (no citations, or no real answer).
    Empty(EmptyVerdict),
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::BadCitation(c) => write!(f, "bad citation {c}"),
            Rejection::Empty(e) => write!(f, "{e}"),
        }
    }
}

/// Everything the synthesizer sees. Citations are validated against this.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Context {
    /// Resolved cards.
    pub cards: Vec<Card>,
    /// CR chunks (category map + BM25 + vector + tool round).
    pub rules: Vec<RuleChunk>,
    /// Scryfall rulings for the cards.
    pub rulings: Vec<Ruling>,
    /// Glossary entries for terms in the Oracle text.
    pub glossary: Vec<GlossaryEntry>,
    /// Prior rated calls, examples only.
    pub prior: Vec<PriorCall>,
    /// Nightmare-card notes.
    pub notes: Vec<CardNote>,
    /// Last N Q&A in the same thread.
    pub history: Vec<Qa>,
    /// Ids of the chunks appended by the synthesizer's `lookup_rules` round;
    /// the retry after a `BadCitation` renders these regardless of its budget.
    #[serde(default)]
    pub tool_round: Vec<RuleId>,
}

impl Context {
    /// Add chunks returned by the `lookup_rules` tool round, skipping ids already present.
    pub fn extend_rules(&mut self, chunks: impl IntoIterator<Item = RuleChunk>) {
        for c in chunks {
            if !self.rules.iter().any(|r| r.id == c.id) {
                self.rules.push(c);
            }
        }
    }

    /// The CR version of the retrieved rule chunks (authoritative for a verdict).
    /// `None` only if Context holds no CR chunks at all.
    #[must_use]
    pub fn cr_version(&self) -> Option<&CrVersion> {
        self.rules.first().map(|r| &r.cr_version)
    }

    /// The rule chunk with this exact id, if present.
    #[must_use]
    pub fn rule(&self, id: &RuleId) -> Option<&RuleChunk> {
        self.rules.iter().find(|r| &r.id == id)
    }

    /// The Scryfall ruling `idx` of `card`, if present.
    #[must_use]
    pub fn ruling(&self, card: CardId, idx: u32) -> Option<&Ruling> {
        self.rulings.iter().find(|r| r.card == card && r.idx == idx)
    }

    /// The prior call with this id, if present.
    #[must_use]
    pub fn prior_call(&self, id: CallId) -> Option<&PriorCall> {
        self.prior.iter().find(|p| p.id == id)
    }
}

/// A rating: 1 incorrect, 2 partially correct, 3 correct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Score {
    /// The answer was wrong.
    Incorrect = 1,
    /// The answer was partially right.
    Partial = 2,
    /// The answer was right.
    Correct = 3,
}

impl Score {
    /// Bayesian-smoothed mean: prior 2.0 with weight 3 (ARCHITECTURE.md §4).
    #[must_use]
    pub fn smoothed_mean(scores: &[Score]) -> f32 {
        const PRIOR: f32 = 2.0;
        const WEIGHT: f32 = 3.0;
        #[allow(clippy::cast_precision_loss)]
        let n = scores.len() as f32;
        let sum: f32 = scores.iter().map(|s| f32::from(*s as u8)).sum();
        (PRIOR * WEIGHT + sum) / (WEIGHT + n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_id_accepts_ascii_forms() {
        for ok in ["702", "702.19", "702.19b", "704.5aa", " 613 "] {
            assert!(RuleId::try_new(ok.to_owned()).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rule_id_and_cr_version_reject_non_ascii_digits() {
        assert!(RuleId::try_new("٧٠٢".to_owned()).is_err());
        assert!(RuleId::try_new("٧٠٢.١٩".to_owned()).is_err());
        assert!(RuleId::try_new("70".to_owned()).is_err());
        assert!(RuleId::try_new("702.19B".to_owned()).is_err());
        assert!(RuleId::try_new("704.5aaa".to_owned()).is_err());
        assert!(CrVersion::try_new("٢٠٢٥٠٨٠١".to_owned()).is_err());
        assert!(CrVersion::try_new("2025080".to_owned()).is_err());
        assert!(CrVersion::try_new("20250801".to_owned()).is_ok());
    }

    #[test]
    fn schemas_carry_no_pattern() {
        for s in [
            schemars::schema_for!(RuleId).to_value(),
            schemars::schema_for!(CrVersion).to_value(),
        ] {
            assert!(s.get("pattern").is_none(), "{s}");
            assert_eq!(s.get("type").and_then(|t| t.as_str()), Some("string"));
        }
    }

    #[test]
    fn citation_displays_for_humans() -> Result<(), Box<dyn std::error::Error>> {
        let c = Citation::Rule { id: RuleId::try_new("702.19b".to_owned())?, quote: "quote".into() };
        assert_eq!(c.to_string(), "rule 702.19b: \"quote\"");
        Ok(())
    }

    fn guess(category: Category) -> CategoryGuess {
        CategoryGuess { category, confidence: Confidence::Low }
    }

    #[test]
    fn categories_dedupe_and_cap_secondary() {
        let e = Extraction {
            card_spans: vec![],
            concepts: vec![],
            primary: guess(Category::Layers),
            secondary: vec![
                guess(Category::Layers),
                guess(Category::Combat),
                guess(Category::Combat),
                guess(Category::Zones),
                guess(Category::Targeting),
            ],
            source: Source::Cr,
        };
        let cats: Vec<Category> = e.categories().map(|g| g.category).collect();
        assert_eq!(cats, vec![Category::Layers, Category::Combat, Category::Zones]);
        assert_eq!(e.primary_category(), Category::Layers);
    }

    #[test]
    fn extraction_schema_requires_primary_and_secondary_is_optional() -> Result<(), serde_json::Error> {
        let s = schemars::schema_for!(Extraction).to_value();
        let required: Vec<&str> = s
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        assert!(required.contains(&"primary"), "{s}");
        assert!(!required.contains(&"secondary"), "{s}");
        let e: Result<Extraction, _> = serde_json::from_str(r#"{"card_spans":[],"concepts":[],"source":"cr"}"#);
        assert!(e.is_err(), "no primary must not deserialize");
        let e: Extraction = serde_json::from_str(
            r#"{"card_spans":[],"concepts":[],"primary":{"category":"layers","confidence":"high"},"source":"cr"}"#,
        )?;
        assert_eq!(e.categories().count(), 1);
        Ok(())
    }

    #[test]
    fn smoothed_mean_uses_prior() {
        assert!((Score::smoothed_mean(&[]) - 2.0).abs() < f32::EPSILON);
        let m = Score::smoothed_mean(&[Score::Correct; 3]);
        assert!((m - 2.5).abs() < 1e-6, "{m}");
    }
}
