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

/// Identity of a Scryfall ruling: a content hash, see [`ruling_key`].
///
/// Scryfall gives rulings no id, only a position in the card's list, and that
/// position shifts whenever a ruling is added or removed ahead of it. A citation
/// keyed by position would then silently point at a different ruling. Keyed by
/// content, a reindexed ruling is the same ruling and a reworded one is a new
/// one — which is what a citation's verbatim quote already assumes.
///
/// Eight bytes, shown and stored as 16 lowercase hex digits. Backing it with
/// bytes rather than a validated string means [`ruling_key`] has no failure
/// path at all: the only way to construct one from text is [`FromStr`], which
/// is where the model's copy of a label is checked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RulingKey([u8; 8]);

/// The text was not 16 hex digits.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("ruling key must be 16 hex digits, got {0:?}")]
pub struct InvalidRulingKey(String);

impl fmt::Display for RulingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for RulingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RulingKey({self})")
    }
}

impl std::str::FromStr for RulingKey {
    type Err = InvalidRulingKey;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let bad = || InvalidRulingKey(s.to_owned());
        if s.len() != 16 || !s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            return Err(bad());
        }
        let mut out = [0u8; 8];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = s.get(2 * i..2 * i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()).ok_or_else(bad)?;
        }
        Ok(Self(out))
    }
}

impl Serialize for RulingKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RulingKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for RulingKey {
    fn schema_name() -> Cow<'static, str> {
        "RulingKey".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            // No "pattern": Anthropic's structured-output subset rejects it.
            "description": "Ruling key: the 16 hex characters from the ruling's label [ruling <key>]"
        })
    }
}

/// The one definition of a ruling's identity: the first 64 bits of SHA-256 over
/// `published_at` (`YYYY-MM-DD`), a newline and `text`.
///
/// Both fields, because Scryfall publishes the same sentence under one card on
/// different dates (~95 cards as of 2026-09) and the two are distinct rulings.
/// The ingest loader computes this on write, the migration that introduced the
/// column computed it in SQL, and a test in the bot crate pins the two together.
#[must_use]
pub fn ruling_key(published_at: &str, text: &str) -> RulingKey {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(published_at.as_bytes());
    hasher.update(b"\n");
    hasher.update(text.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let [k0, k1, k2, k3, k4, k5, k6, k7, ..] = digest;
    RulingKey([k0, k1, k2, k3, k4, k5, k6, k7])
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
    /// A word-suffix or single word of a multi-word span is a nickname in the
    /// alias table and nothing else in the span is ("mirage LED" → `led`):
    /// a printing / set / frame qualifier in front of a nickname.
    AliasSuffix,
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
    /// A Scryfall ruling of `card`, identified by content ([`ruling_key`]).
    ScryfallRuling {
        /// The card.
        card: CardId,
        /// The ruling's key, copied from its `[ruling <key>]` label.
        ruling: RulingKey,
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
    /// The current Oracle text of face `face` of `card`, for answers that
    /// hinge on the card's current wording (errata, "what does it do now").
    /// The face *name* is not quotable: a name carries no rule content, so a
    /// name-only citation would satisfy "cite something" with nothing.
    OracleText {
        /// The card.
        card: CardId,
        /// Face index (0 for single-faced cards).
        face: u32,
        /// Verbatim span of that face's Oracle text.
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
            | Citation::PriorCall { quote, .. }
            | Citation::OracleText { quote, .. } => quote,
        }
    }
}

/// Longest prefix of a malformed citation's raw JSON that is kept. It is
/// rendered into the retry prompt and logged, so it is bounded; counted in
/// `char`s, since the model's quotes routinely carry the CR's curly
/// apostrophes and em dashes.
pub const MALFORMED_RAW_CHARS: usize = 300;

/// A citation the model emitted that could not be parsed into a [`Citation`]
/// at all: an unknown `kind`, a missing field, or — the case this was built
/// for — a field that failed its own newtype's validator, such as an empty
/// [`RuleId`](crate::RuleId).
///
/// Such an element used to fail the whole payload at the serde boundary,
/// which surfaced as [`JudgeError::Upstream`](crate::JudgeError::Upstream) —
/// a variant `judge()` does not retry — so one stub citation discarded an
/// otherwise sound answer. Keeping it as data instead makes it an ordinary
/// rejection that the existing single retry can explain to the model.
/// [`Verdict<Validated>`](crate::Verdict) can never hold one: `validate` is
/// the only route to that state and it refuses while any are present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MalformedCitation {
    /// The element as the model wrote it, as compact JSON: at most
    /// [`MALFORMED_RAW_CHARS`] characters, plus a trailing `…` if it was cut.
    pub raw: String,
    /// The deserialization error, e.g. `RuleId violated the regular expression`.
    pub error: String,
}

impl MalformedCitation {
    /// Record a failed element. `raw` is truncated on a `char` boundary.
    #[must_use]
    pub fn new(raw: &str, error: &str) -> Self {
        let mut kept: String = raw.chars().take(MALFORMED_RAW_CHARS).collect();
        if kept.len() < raw.len() {
            kept.push('…');
        }
        Self { raw: kept, error: error.to_owned() }
    }
}

impl fmt::Display for MalformedCitation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unreadable citation {}: {}", self.raw, self.error)
    }
}

impl fmt::Display for Citation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Citation::Rule { id, quote } => write!(f, "rule {id}: {quote:?}"),
            Citation::ScryfallRuling { card, ruling, quote } => write!(f, "ruling {card}/{ruling}: {quote:?}"),
            Citation::PriorCall { id, quote } => write!(f, "prior call {id}: {quote:?}"),
            Citation::OracleText { card, face, quote } => write!(f, "oracle {card}#{face}: {quote:?}"),
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

impl Face {
    /// True if `quote` appears verbatim in the Oracle text (not the name).
    #[must_use]
    pub fn contains_quote(&self, quote: &str) -> bool {
        self.oracle_text.contains(quote)
    }
}

impl Card {
    /// Face `idx` (0-based, in Scryfall face order), if it exists.
    #[must_use]
    pub fn face(&self, idx: u32) -> Option<&Face> {
        usize::try_from(idx).ok().and_then(|i| self.faces.get(i))
    }
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
    /// Content key, [`ruling_key`] of `published_at` and `text`.
    pub key: RulingKey,
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
    /// A citation could not be parsed at all.
    Malformed(MalformedCitation),
    /// The verdict was empty (no citations, or no real answer).
    Empty(EmptyVerdict),
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::BadCitation(c) => write!(f, "bad citation {c}"),
            Rejection::Malformed(m) => write!(f, "{m}"),
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

    /// The card with this oracle id, if present.
    #[must_use]
    pub fn card(&self, id: CardId) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }

    /// The Scryfall ruling of `card` with this key, if present.
    #[must_use]
    pub fn ruling(&self, card: CardId, key: &RulingKey) -> Option<&Ruling> {
        self.rulings.iter().find(|r| r.card == card && &r.key == key)
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
    fn ruling_key_round_trips_and_rejects_bad_text() {
        let k = ruling_key("2019-10-04", "Stomp can target a player.");
        let shown = k.to_string();
        assert_eq!(shown.len(), 16);
        assert!(shown.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(shown.parse::<RulingKey>(), Ok(k));
        assert_eq!(serde_json::from_str::<RulingKey>(&format!("\" {shown} \"")).ok(), Some(k), "whitespace is trimmed like the model's other ids");
        assert_eq!(serde_json::to_string(&k).ok(), Some(format!("\"{shown}\"")));
        for bad in ["", "0123456789abcde", "0123456789abcdef0", "0123456789ABCDEF", "0123456789abcdeg"] {
            assert!(bad.parse::<RulingKey>().is_err(), "{bad:?}");
        }
        // Same text on another date is another ruling; same inputs are the same key.
        assert_ne!(ruling_key("2019-10-05", "Stomp can target a player."), k);
        assert_eq!(ruling_key("2019-10-04", "Stomp can target a player."), k);
    }

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
        let o = Citation::OracleText { card: CardId::new(Uuid::from_u128(7)), face: 1, quote: "q".into() };
        assert_eq!(o.to_string(), "oracle 00000000-0000-0000-0000-000000000007#1: \"q\"");
        assert_eq!(o.quote(), "q");
        // Serde tag is consistent with the other variants.
        let v = serde_json::to_value(&o)?;
        assert_eq!(v.get("kind").and_then(|k| k.as_str()), Some("oracle_text"));
        assert_eq!(v.get("face").and_then(serde_json::Value::as_u64), Some(1));
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
