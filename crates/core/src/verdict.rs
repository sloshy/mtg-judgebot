//! `Verdict<Unvalidated | Validated>` — only a validated verdict can be
//! persisted or rendered (invariant I3). The only way to obtain
//! `Verdict<Validated>` is `Verdict::<Unvalidated>::validate`.
//!
//! Enforced by construction:
//! * `Deserialize` is implemented for `Verdict<Unvalidated>` **only**, so
//!   `serde_json::from_str::<Verdict<Validated>>(..)` does not compile.
//! * `Validated` carries the `cr_version` and the resolved cards stamped from
//!   Context and has private fields, so no code outside `validate` can build
//!   one — and every reply that renders a verdict can name the cards it was
//!   about without being handed the Context separately.
//! * The model never reports `cr_version` or `source`: neither is in the
//!   model-facing schema. `cr_version` is taken from the retrieved CR chunks
//!   and `source` from the extraction, as an [`AnswerableSource`], so a
//!   verdict cannot claim to be out of scope to dodge the citation check.
//! * Each state names its own citation container ([`State::Citations`]).
//!   `Unvalidated` uses [`Citations`], which can hold elements that did not
//!   parse; `Validated` uses `Vec<Citation>`, which cannot. `Citations` keeps
//!   its fields private and offers exactly one way out — `into_clean`, which
//!   fails on the first unreadable element — so `validate` cannot build a
//!   validated verdict without having checked. Deleting the check is a type
//!   error, not a silent regression.

use std::{borrow::Cow, sync::LazyLock};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use nonempty::NonEmpty;
use regex::Regex;

use crate::{
    AnswerableSource, CardRef, Category, Citation, Confidence, Context, CrVersion, EmptyVerdict,
    JudgeError, MalformedCitation, Quote, RuleId, Source, UncitedRules, quote,
};

/// An answer shorter than this (in characters, trimmed) is not an answer:
/// `"pending"`, `"see above"` and the like fail validation as
/// [`EmptyVerdict::ShortAnswer`].
pub const MIN_ANSWER_CHARS: usize = 40;

/// Longest answer an agent-driven session accepts, in characters. Discord's
/// embed description holds 4096; the prompt asks for about 1500. The Anthropic
/// path needs no such check (`max_tokens` bounds it), so this is enforced by
/// the session, not by `validate`.
pub const MAX_ANSWER_CHARS: usize = 4000;

mod sealed {
    pub trait Sealed {}
}

/// How a verdict in a given state stores its citations. Sealed: the only two
/// containers are [`Citations`] (which may hold unreadable elements) and
/// `Vec<Citation>` (which cannot).
pub trait CitationList:
    sealed::Sealed + Clone + core::fmt::Debug + PartialEq + Serialize + Send + Sync + 'static
{
    /// The citations that parsed, in the model's order.
    fn as_slice(&self) -> &[Citation];
}

/// Validation state marker. Sealed: only `Unvalidated` and `Validated` exist.
///
/// The associated `Citations` type is what makes "a validated verdict holds no
/// unreadable citation" a *type* fact rather than a convention: `Validated`
/// stores a plain `Vec<Citation>`, which has nowhere to put a
/// [`MalformedCitation`], so `validate` cannot build one without first going
/// through [`Citations::into_clean`] — the only bridge between the two, and one
/// that fails if anything was unreadable.
pub trait State:
    sealed::Sealed + Clone + core::fmt::Debug + PartialEq + Serialize + Send + Sync + 'static
{
    /// Citation container for this state.
    type Citations: CitationList;
}

/// Fresh from the model; citations not yet checked against Context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Unvalidated {}

/// Every citation exists in Context and every quote is a substring of its
/// source. Carries the CR version the context was retrieved under and the
/// (answerable) source the extraction classified the question into.
/// Deliberately **not** `Deserialize` and not constructible outside `validate`:
///
/// ```compile_fail
/// let _: judge_core::Verdict<judge_core::Validated> = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Validated {
    cr_version: CrVersion,
    source: AnswerableSource,
    cards: Vec<CardRef>,
}

impl sealed::Sealed for Unvalidated {}
impl sealed::Sealed for Validated {}
impl sealed::Sealed for Citations {}
impl sealed::Sealed for Vec<Citation> {}

impl State for Unvalidated {
    type Citations = Citations;
}
/// A validated verdict's citations are a plain `Vec`: the malformed-citation
/// state is *unrepresentable* here, not merely checked for.
impl State for Validated {
    type Citations = Vec<Citation>;
}

impl CitationList for Citations {
    fn as_slice(&self) -> &[Citation] {
        &self.ok
    }
}
impl CitationList for Vec<Citation> {
    fn as_slice(&self) -> &[Citation] {
        self
    }
}

/// The model's citation array, parsed one element at a time: those that became
/// a [`Citation`], and those that could not.
///
/// A plain `Vec<Citation>` makes the array all-or-nothing, and the failure
/// lands at the serde boundary — inside `parse_verdict`, before `validate` is
/// ever reached — so it becomes `JudgeError::Upstream`, which `judge()` does
/// not retry. One stub element therefore discarded the whole answer. Per
/// element, an unreadable citation is just another rejection, and the existing
/// single retry gets to tell the model what was wrong.
///
/// This is a *parsing* leniency, never a validation one: [`Citations::into_clean`]
/// is the only way to reach the `Vec<Citation>` a [`Validated`] verdict is made
/// of, and it fails if anything was unreadable.
///
/// Serializes as a bare array of the citations that parsed — lossy by
/// construction, which is safe only because nothing round-trips an
/// *unvalidated* verdict through serde: production serializes
/// `Verdict<Validated>` (whose citations are already a plain `Vec`) and the
/// `&[Citation]` from `citations()`. Deserializing it back into
/// `Verdict<Unvalidated>` would launder away the malformed entries, so don't
/// add such a path.
///
/// Two constraints worth knowing before reusing this type:
/// * **JSON only.** Routing elements through `serde_json::Value` uses
///   `deserialize_any`, so a verdict can no longer be read from a
///   non-self-describing format (bincode, postcard). Nothing needs that today.
/// * **Duplicate keys are no longer loud.** `serde_json::Map` de-duplicates on
///   insert, so `{"kind":"rule","id":"","id":"702.19","quote":"q"}` now parses
///   as the last value wins where a derived `Vec<Citation>` reported
///   `duplicate field`. The surviving citation is still checked in full against
///   `Context`, so this costs loudness, not soundness.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Citations {
    ok: Vec<Citation>,
    malformed: Vec<MalformedCitation>,
    /// Unreadable elements that quote nothing (see [`stub_reason`]). They are
    /// set aside rather than rejected: [`Verdict::validate`] drops them (D21).
    stubs: Vec<MalformedCitation>,
}

impl Citations {
    /// The bridge to a [`Validated`] verdict's citation list, and the reason
    /// `validate` cannot *forget* to check: `Verdict<Validated>` is built from
    /// a `Vec<Citation>`, and dropping this call leaves a `Citations` where a
    /// `Vec` is required, which does not compile.
    ///
    /// It is not a capability boundary — someone determined could write
    /// `as_slice().to_vec()` — but omission is the failure mode that actually
    /// happens, and omission is now a type error.
    ///
    /// Returns the readable citations and the stubs that were set aside.
    ///
    /// # Errors
    /// The first unreadable element that is not a stub, if any.
    fn into_clean(self) -> Result<(Vec<Citation>, Vec<MalformedCitation>), MalformedCitation> {
        match self.malformed.into_iter().next() {
            Some(m) => Err(m),
            None => Ok((self.ok, self.stubs)),
        }
    }
}

impl<'de> Deserialize<'de> for Citations {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut out = Self::default();
        for v in Vec::<serde_json::Value>::deserialize(d)? {
            // `&Value` is itself a Deserializer, so the element is parsed
            // without cloning it; `v` stays intact for the error message.
            match Citation::deserialize(&v) {
                Ok(c) => out.ok.push(c),
                Err(e) => {
                    let m = MalformedCitation::new(&v.to_string(), &e.to_string());
                    // An element whose quote is there and quotes nothing is
                    // a stub whatever else is wrong with it. Anything else
                    // that cannot be read may be a citation the model meant:
                    // a real quote, or one under a misspelt or mistyped
                    // `quote` key, which an unconstrained backend or an
                    // outside agent can produce.
                    let quotes_nothing = v
                        .get("quote")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|q| stub_reason(q).is_some());
                    if quotes_nothing {
                        out.stubs.push(m);
                    } else {
                        out.malformed.push(m);
                    }
                }
            }
        }
        Ok(out)
    }
}

impl Serialize for Citations {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.ok.serialize(s)
    }
}

/// Identical to `Vec<Citation>`: the model is shown no trace of the leniency,
/// which lives entirely on the reading side.
impl JsonSchema for Citations {
    fn schema_name() -> Cow<'static, str> {
        <Vec<Citation>>::schema_name()
    }
    fn schema_id() -> Cow<'static, str> {
        <Vec<Citation>>::schema_id()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        <Vec<Citation>>::json_schema(g)
    }
    fn inline_schema() -> bool {
        <Vec<Citation>>::inline_schema()
    }
}

/// The model-facing shape: what the synthesizer's structured output must match.
/// Private so that `Deserialize` can only be reached through `Verdict<Unvalidated>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Verdict")]
struct VerdictData<C> {
    /// The answer, in plain prose, quoting current Oracle text where relevant.
    answer: String,
    /// Self-assessed confidence.
    confidence: Confidence,
    /// Each citation quotes a span verbatim from a rule, ruling or prior call in Context.
    citations: C,
    /// The category that best fits the question.
    category: Category,
}

/// The synthesizer's structured answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(bound = "S: State")]
pub struct Verdict<S: State = Unvalidated> {
    #[serde(flatten)]
    data: VerdictData<S::Citations>,
    #[serde(flatten)]
    state: S,
}

impl<S: State> Verdict<S> {
    /// The answer text.
    #[must_use]
    pub fn answer(&self) -> &str {
        &self.data.answer
    }
    /// Self-reported confidence.
    #[must_use]
    pub fn confidence(&self) -> Confidence {
        self.data.confidence
    }
    /// Citations, in the model's order. Unreadable elements are not here; on a
    /// `Verdict<Validated>` there were none, because its citation list is a
    /// `Vec<Citation>` that cannot hold one.
    #[must_use]
    pub fn citations(&self) -> &[Citation] {
        self.data.citations.as_slice()
    }
    /// Category assigned by the model.
    #[must_use]
    pub fn category(&self) -> Category {
        self.data.category
    }
}

impl Verdict<Unvalidated> {
    /// Construct an unvalidated verdict (adapters and tests).
    #[must_use]
    pub fn new(
        answer: String,
        confidence: Confidence,
        citations: Vec<Citation>,
        category: Category,
    ) -> Self {
        let citations = Citations {
            ok: citations,
            malformed: vec![],
            stubs: vec![],
        };
        Self {
            data: VerdictData {
                answer,
                confidence,
                citations,
                category,
            },
            state: Unvalidated {},
        }
    }

    /// Check the verdict against `ctx`:
    /// (0) every citation the model wrote was at least *readable*;
    /// (1) the answer is at least [`MIN_ANSWER_CHARS`] long and cites
    ///     something at all;
    /// (a) each referenced rule / ruling / prior call / card face exists in
    ///     Context, and
    /// (b) each quote is a non-empty substring of that source (for Oracle
    ///     text: the face's current text, never its name), compared with
    ///     typographic punctuation folded and then *replaced* by the source's
    ///     own span, so what is stored is exact ([`crate::quote`]);
    /// then stamp the CR version of the retrieved chunks and `source`, which
    /// is the extraction's classification (only answerable sources reach
    /// synthesis, and the type says so).
    ///
    /// # Errors
    /// `JudgeError::MalformedCitation` for (0), `JudgeError::EmptyVerdict` for
    /// (1), `JudgeError::BadCitation` carrying the first offending citation
    /// for (a)/(b), or `JudgeError::Upstream` if Context holds no CR chunks
    /// (the category map always injects some, so this indicates a broken
    /// retriever).
    ///
    /// (0) comes first deliberately: an unreadable element is also the likely
    /// reason the parsed citations are missing or thin, so reporting it beats
    /// reporting the symptom (`NoCitations`) it caused.
    pub fn validate(
        self,
        ctx: &Context,
        source: AnswerableSource,
    ) -> Result<Verdict<Validated>, JudgeError> {
        // Check (0). Not a courtesy ordering: `Validated`'s citation list is a
        // `Vec<Citation>`, and this is the only way to get one, so the check
        // cannot be skipped or reordered away without failing to compile.
        let (citations, mut stubs) = self
            .data
            .citations
            .into_clean()
            .map_err(JudgeError::MalformedCitation)?;
        // Check (0b), D21: a citation that quotes nothing asserts nothing, so
        // it is dropped rather than held against the answer. Everything that
        // is left is checked as strictly as ever, at least one real citation
        // is still required, and check (c) catches prose that leaned on a
        // dropped rule by number. Only an answer with nothing *but* stubs is
        // rejected for them, so the retry is told what it did.
        let (placeholders, citations): (Vec<_>, Vec<_>) = citations
            .into_iter()
            .partition(|c| placeholder(c).is_some());
        stubs.extend(placeholders.iter().filter_map(placeholder));
        if let Some(first) = stubs.first() {
            if citations.is_empty() {
                return Err(JudgeError::MalformedCitation(first.clone()));
            }
            tracing::info!(dropped = stubs.len(), first = %first, "stub citations dropped");
        }
        if let Some(e) = emptiness(&self.data.answer, &citations) {
            return Err(JudgeError::EmptyVerdict(e));
        }
        let citations = requote(citations, ctx).map_err(JudgeError::BadCitation)?;
        // Check (c), last: only once every citation is known good is "the
        // prose names a rule nothing cites" the thing worth telling the model.
        if let Some(uncited) = uncited_rules(&self.data.answer, &citations) {
            return Err(JudgeError::UncitedRules(uncited));
        }
        let cr_version = ctx.cr_version().cloned().ok_or_else(|| {
            anyhow::anyhow!("context holds no CR chunks; cannot stamp cr_version")
        })?;
        let data = VerdictData {
            answer: self.data.answer,
            confidence: self.data.confidence,
            citations,
            category: self.data.category,
        };
        Ok(Verdict {
            data,
            state: Validated {
                cr_version,
                source,
                cards: ctx.cards.iter().map(CardRef::from).collect(),
            },
        })
    }
}

/// A rule number as prose writes it: three digits, a dot, digits, and up to
/// two letters. The regex crate has no lookaround, so what stands before the
/// match is checked by hand in [`uncited_rules`].
static PROSE_RULE_ID: LazyLock<Regex> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "the pattern is a constant: an invalid one panics in every test that uses it"
    )]
    Regex::new(r"\b[1-9][0-9]{2}\.[0-9]+[a-z]{0,2}\b").expect("PROSE_RULE_ID is a valid regex")
});

/// `702.19b` → `702.19`; a rule-level id is its own rule.
fn rule_of(id: &str) -> &str {
    id.trim_end_matches(|c: char| c.is_ascii_lowercase())
}

/// The rule numbers `answer` names that no rule citation covers, if any.
/// A citation covers a number when it cites that id, its rule, or one of its
/// sub-rules. A number that is part of a longer figure (`$100.50`, `1.702.19`)
/// or that is not a well-formed rule id is not a rule number.
fn uncited_rules(answer: &str, citations: &[Citation]) -> Option<UncitedRules> {
    let cited: Vec<&str> = citations
        .iter()
        .filter_map(|c| match c {
            Citation::Rule { id, .. } => Some(id.as_ref()),
            Citation::ScryfallRuling { .. }
            | Citation::PriorCall { .. }
            | Citation::OracleText { .. } => None,
        })
        .collect();
    let mut missing: Vec<RuleId> = Vec::new();
    for m in PROSE_RULE_ID.find_iter(answer) {
        let before = answer.get(..m.start()).and_then(|s| s.chars().next_back());
        if before.is_some_and(|c| c == '$' || c == '.') {
            continue;
        }
        let Ok(id) = RuleId::try_new(m.as_str().to_owned()) else {
            continue;
        };
        let covered = cited
            .iter()
            .any(|c| *c == id.as_ref() || rule_of(c) == id.as_ref() || *c == rule_of(id.as_ref()));
        if !covered && !missing.contains(&id) {
            missing.push(id);
        }
    }
    NonEmpty::from_vec(missing).map(UncitedRules::new)
}

/// Quotes a model writes when it has nothing to quote. Compared after
/// trimming, lowercasing and dropping surrounding punctuation.
const STUB_QUOTES: &[&str] = &[
    "placeholder",
    "quote",
    "text",
    "citation",
    "n/a",
    "na",
    "none",
    "null",
    "tbd",
    "todo",
    "unknown",
];

/// Shortest quote that can name a span of anything. Real citations run to a
/// clause at least; `x` and `...` are what a model emits to fill a required
/// field.
pub const MIN_QUOTE_CHARS: usize = 4;

/// `c` as a [`MalformedCitation`] if its quote is a placeholder rather than a
/// quotation: a stock word, or too short to quote anything. Judged on the
/// quote alone. A real quote under a wrong id is a bad citation, which has a
/// more useful notice (see [`misfiled_oracle_text`]).
fn placeholder(c: &Citation) -> Option<MalformedCitation> {
    stub_reason(c.quote()).map(|why| MalformedCitation::new(&c.to_string(), why))
}

/// Why `quote` quotes nothing, if it does: blank, a stock word, or too short.
fn stub_reason(quote: &str) -> Option<&'static str> {
    let quote = quote.trim();
    let bare = quote
        .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '/')
        .to_lowercase();
    if quote.is_empty() {
        Some("the quote is empty")
    } else if STUB_QUOTES.contains(&bare.as_str()) {
        Some("the quote is a placeholder, not text from a source")
    } else if quote.chars().count() < MIN_QUOTE_CHARS {
        Some("the quote is too short to be text from a source")
    } else {
        None
    }
}

/// The face of its own card whose Oracle text a *ruling* citation actually
/// quotes, when `ctx` holds no such ruling.
///
/// A card with no rulings in the material still has Oracle text, and a model
/// that wants to cite "what the card says now" sometimes files that under
/// `scryfall_ruling` with an invented key. The quote is genuine, so the useful
/// thing to tell it is which kind it meant, not that the ruling is missing.
/// Never used to repair a citation: a verdict is admitted as written or not
/// at all.
#[must_use]
pub fn misfiled_oracle_text(c: &Citation, ctx: &Context) -> Option<u32> {
    let Citation::ScryfallRuling {
        card,
        ruling,
        quote,
    } = c
    else {
        return None;
    };
    if ctx.ruling(*card, ruling).is_some() {
        return None;
    }
    let q = quote.as_ref().trim();
    ctx.card(*card)?
        .faces
        .iter()
        .position(|f| f.locate_quote(q).is_some())
        .and_then(|i| u32::try_from(i).ok())
}

/// Why a verdict counts as empty, if it does (check (1) of `validate`).
fn emptiness(answer: &str, citations: &[Citation]) -> Option<EmptyVerdict> {
    let chars = answer.trim().chars().count();
    if chars < MIN_ANSWER_CHARS {
        return Some(EmptyVerdict::ShortAnswer { chars });
    }
    if citations.is_empty() {
        return Some(EmptyVerdict::NoCitations);
    }
    None
}

impl Verdict<Validated> {
    /// CR version the verdict was validated against (from the retrieved chunks).
    #[must_use]
    pub fn cr_version(&self) -> &CrVersion {
        &self.state.cr_version
    }
    /// Rules body the answer draws on, as classified by the extraction.
    #[must_use]
    pub fn source(&self) -> Source {
        self.state.source.into()
    }
    /// The cards the question's names resolved to, in resolution order.
    #[must_use]
    pub fn cards(&self) -> &[CardRef] {
        &self.state.cards
    }
}

/// Only the unvalidated state can be deserialized: model output and stored
/// JSON both come back as `Verdict<Unvalidated>` and must pass `validate`.
impl<'de> Deserialize<'de> for Verdict<Unvalidated> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        VerdictData::deserialize(d).map(|data| Verdict {
            data,
            state: Unvalidated {},
        })
    }
}

/// The model-facing schema (used for `output_config.format`).
impl JsonSchema for Verdict<Unvalidated> {
    fn schema_name() -> Cow<'static, str> {
        <VerdictData<Citations>>::schema_name()
    }
    fn schema_id() -> Cow<'static, str> {
        <VerdictData<Citations>>::schema_id()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        <VerdictData<Citations>>::json_schema(g)
    }
    fn inline_schema() -> bool {
        <VerdictData<Citations>>::inline_schema()
    }
}

/// Whether `c` is supported by `ctx`: its referenced rule, ruling, prior call
/// or card face is present there and its quote is a non-empty verbatim
/// substring of that source. This is the (a)/(b) check of [`Verdict::validate`]
/// on one citation, exported so the retirement pass can ask the same question
/// of a stored call against *today's* data: a call is live exactly while every
/// citation it was admitted with would still be admitted.
#[must_use]
pub fn citation_supported(c: &Citation, ctx: &Context) -> bool {
    source_quote(c, ctx).is_some()
}

/// The *source's* text for the span `c` quotes, if `ctx` supports `c`.
///
/// Matching folds typographic punctuation ([`quote::locate`]), so this is not
/// always the string the model wrote; [`Verdict::validate`] stores what comes
/// back rather than what came in, which is why a stored quote is always a
/// byte-exact substring of its source and [`citation_supported`] stays a
/// strict check when the retirement pass re-runs it.
#[must_use]
fn source_quote(c: &Citation, ctx: &Context) -> Option<Quote> {
    let q = c.quote().trim();
    if q.is_empty() {
        return None;
    }
    let found = match c {
        Citation::Rule { id, .. } => ctx.rule(id).and_then(|r| r.locate_quote(q)),
        Citation::ScryfallRuling { card, ruling, .. } => ctx
            .ruling(*card, ruling)
            .and_then(|r| quote::locate(&r.text, q)),
        Citation::PriorCall { id, .. } => ctx
            .prior_call(*id)
            .and_then(|p| quote::locate(&p.answer, q)),
        Citation::OracleText { card, face, .. } => ctx
            .card(*card)
            .and_then(|c| c.face(*face))
            .and_then(|f| f.locate_quote(q)),
    };
    // The span has the trimmed quote's length and starts and ends on a
    // non-space character, so it is never blank; `ok()` is not a leniency.
    found.and_then(|span| Quote::try_new(span).ok())
}

/// Replace each citation's quote with the source's own text for it, or report
/// the first citation no source supports. Check (a)/(b) of
/// [`Verdict::validate`], and the only place a quote is rewritten.
fn requote(citations: Vec<Citation>, ctx: &Context) -> Result<Vec<Citation>, Citation> {
    citations
        .into_iter()
        .map(|c| match source_quote(&c, ctx) {
            Some(exact) => Ok(c.with_quote(exact)),
            None => Err(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallId, Card, CardId, Face, Layout, RuleChunk, RuleId, Ruling, ruling_key};
    use nonempty::NonEmpty;
    use uuid::Uuid;

    fn waylay_id() -> CardId {
        CardId::new(Uuid::from_u128(9))
    }

    fn waylay() -> Card {
        let face = |name: &str, text: &str| Face {
            name: name.into(),
            oracle_text: text.into(),
            mana_cost: "{2}{W}".into(),
            type_line: "Instant".into(),
        };
        Card {
            id: waylay_id(),
            name: "Waylay".into(),
            layout: Layout::Normal,
            faces: NonEmpty::from((
                face(
                    "Waylay",
                    "Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step.",
                ),
                vec![face("Back", "Nothing here.")],
            )),
        }
    }

    fn rule(id: &str, body: &str) -> Result<RuleChunk, Box<dyn std::error::Error>> {
        Ok(RuleChunk {
            id: RuleId::try_new(id.to_owned())?,
            parent_id: None,
            subsection: RuleId::try_new("702".to_owned())?,
            heading: "Lifelink".into(),
            body: body.into(),
            examples: vec!["Example: something.".into()],
            cr_version: CrVersion::try_new("20250801".to_owned())?,
        })
    }

    fn ctx() -> Result<Context, Box<dyn std::error::Error>> {
        let card = CardId::new(Uuid::from_u128(7));
        Ok(Context {
            cards: vec![waylay()],
            rules: vec![rule(
                "702.15b",
                "Damage dealt by a source with lifelink causes that source's controller to gain that much life.",
            )?],
            rulings: vec![Ruling {
                card,
                key: ruling_key("2020-01-01", "Lifelink is not a triggered ability."),
                published_at: "2020-01-01".into(),
                text: "Lifelink is not a triggered ability.".into(),
            }],
            ..Context::default()
        })
    }

    const ANSWER: &str = "You gain life simultaneously with the damage being dealt.";

    fn verdict(citations: Vec<Citation>) -> Verdict<Unvalidated> {
        Verdict::new(
            ANSWER.into(),
            Confidence::High,
            citations,
            Category::KeywordAbilities,
        )
    }

    const CR: AnswerableSource = AnswerableSource::Cr;

    fn good_citation() -> Result<Citation, Box<dyn std::error::Error>> {
        Ok(Citation::Rule {
            id: RuleId::try_new("702.15b".to_owned())?,
            quote: Quote::try_new("gain that much life")?,
        })
    }

    #[test]
    fn accepts_valid_citations_and_stamps_cr_version() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let v = verdict(vec![
            Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: Quote::try_new("gain that much life")?,
            },
            Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: Quote::try_new("Example: something.")?,
            },
            Citation::ScryfallRuling {
                card: CardId::new(Uuid::from_u128(7)),
                ruling: ruling_key("2020-01-01", "Lifelink is not a triggered ability."),
                quote: Quote::try_new("not a triggered ability")?,
            },
        ]);
        let ok = v.validate(&c, CR).map_err(|e| e.to_string())?;
        assert_eq!(ok.citations().len(), 3);
        assert_eq!(ok.confidence(), Confidence::High);
        assert_eq!(ok.cr_version().as_ref(), "20250801");
        assert_eq!(ok.source(), Source::Cr);
        Ok(())
    }

    /// The CR and Scryfall are typeset with curly apostrophes and em dashes,
    /// and models retype them as ASCII. Such a citation is admitted — and
    /// stored carrying the *source's* typography, so what is persisted and
    /// re-checked by the retirement pass is still byte-exact.
    #[test]
    fn ascii_punctuation_is_admitted_and_snapped_back_to_the_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let card = CardId::new(Uuid::from_u128(7));
        let text =
            "Lifelink isn\u{2019}t a triggered ability \u{2014} it\u{2019}s a static ability.";
        let c = Context {
            rules: vec![rule(
                "702.15b",
                "A source\u{2019}s controller gains that much life.",
            )?],
            rulings: vec![Ruling {
                card,
                key: ruling_key("2020-01-01", text),
                published_at: "2020-01-01".into(),
                text: text.into(),
            }],
            ..Context::default()
        };
        let v = verdict(vec![
            Citation::Rule {
                id: RuleId::try_new("702.15b".to_owned())?,
                quote: Quote::try_new("A source's controller")?,
            },
            Citation::ScryfallRuling {
                card,
                ruling: ruling_key("2020-01-01", text),
                quote: Quote::try_new("isn't a triggered ability - it's a static")?,
            },
        ]);
        let ok = v.validate(&c, CR).map_err(|e| e.to_string())?;
        let quotes: Vec<&str> = ok.citations().iter().map(Citation::quote).collect();
        assert_eq!(
            quotes,
            vec![
                "A source\u{2019}s controller",
                "isn\u{2019}t a triggered ability \u{2014} it\u{2019}s a static"
            ]
        );
        // And the stored form is what `citation_supported` will check later.
        assert!(ok.citations().iter().all(|x| citation_supported(x, &c)));
        Ok(())
    }

    #[test]
    fn rejects_reference_not_in_context() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let bad = Citation::Rule {
            id: RuleId::try_new("702.19".to_owned())?,
            quote: Quote::try_new("gain that much life")?,
        };
        let err = verdict(vec![bad.clone()]).validate(&c, CR).err();
        assert!(
            matches!(err, Some(JudgeError::BadCitation(ref x)) if *x == bad),
            "{err:?}"
        );

        let bad_ruling = Citation::ScryfallRuling {
            card: CardId::new(Uuid::from_u128(7)),
            ruling: ruling_key("2020-01-01", "some other ruling"),
            quote: Quote::try_new("Lifelink")?,
        };
        assert!(matches!(
            verdict(vec![bad_ruling]).validate(&c, CR),
            Err(JudgeError::BadCitation(_))
        ));

        let bad_prior = Citation::PriorCall {
            id: CallId::new(Uuid::from_u128(1)),
            quote: Quote::try_new("lifelink stacks")?,
        };
        assert!(matches!(
            verdict(vec![bad_prior]).validate(&c, CR),
            Err(JudgeError::BadCitation(_))
        ));
        Ok(())
    }

    #[test]
    fn rejects_quote_not_substring() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let bad = Citation::Rule {
            id: RuleId::try_new("702.15b".to_owned())?,
            quote: Quote::try_new("gain twice that much life")?,
        };
        assert!(matches!(
            verdict(vec![bad]).validate(&c, CR),
            Err(JudgeError::BadCitation(_))
        ));
        // A blank quote cannot be built at all; see `a_blank_quote_is_unreadable`.
        assert!(Quote::try_new("").is_err() && Quote::try_new(" \t\n\u{00A0}").is_err());
        assert_eq!(Quote::try_new(" x ")?.as_ref(), " x ");
        Ok(())
    }

    #[test]
    fn rejects_context_without_rules() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            verdict(vec![good_citation()?]).validate(&Context::default(), CR),
            Err(JudgeError::BadCitation(_))
        ));
        // Nothing to cite and nothing to stamp: the empty-citations check comes first.
        assert!(matches!(
            verdict(vec![]).validate(&Context::default(), CR),
            Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))
        ));
        Ok(())
    }

    #[test]
    fn rejects_empty_verdicts() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        // No citations for a CR answer.
        assert!(matches!(
            verdict(vec![]).validate(&c, CR),
            Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))
        ));
        // No citations for a Commander answer.
        let cmd = Verdict::new(ANSWER.into(), Confidence::High, vec![], Category::Commander);
        assert!(matches!(
            cmd.validate(&c, AnswerableSource::Commander),
            Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))
        ));
        // A "pending" answer, even with a valid citation.
        let short = Verdict::new(
            "pending".into(),
            Confidence::High,
            vec![good_citation()?],
            Category::KeywordAbilities,
        );
        assert!(matches!(
            short.validate(&c, CR),
            Err(JudgeError::EmptyVerdict(EmptyVerdict::ShortAnswer {
                chars: 7
            }))
        ));
        // Whitespace does not count.
        let blank = Verdict::new(
            " ".repeat(50),
            Confidence::High,
            vec![good_citation()?],
            Category::KeywordAbilities,
        );
        assert!(matches!(
            blank.validate(&c, CR),
            Err(JudgeError::EmptyVerdict(EmptyVerdict::ShortAnswer {
                chars: 0
            }))
        ));
        // Exactly the minimum passes.
        let exact = Verdict::new(
            "x".repeat(MIN_ANSWER_CHARS),
            Confidence::High,
            vec![good_citation()?],
            Category::KeywordAbilities,
        );
        assert!(exact.validate(&c, CR).is_ok());
        // Commander verdicts carry their source through validation.
        let cmd = Verdict::new(
            ANSWER.into(),
            Confidence::High,
            vec![good_citation()?],
            Category::Commander,
        );
        assert_eq!(
            cmd.validate(&c, AnswerableSource::Commander)?.source(),
            Source::Commander
        );
        Ok(())
    }

    #[test]
    fn oracle_text_citations() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let waylay = waylay_id();
        // Valid: a substring of face 0's Oracle text; of face 1's.
        let ok = verdict(vec![
            Citation::OracleText {
                card: waylay,
                face: 0,
                quote: Quote::try_new("Exile them at the beginning of the next cleanup step.")?,
            },
            Citation::OracleText {
                card: waylay,
                face: 1,
                quote: Quote::try_new("Nothing here")?,
            },
        ]);
        assert_eq!(
            ok.validate(&c, CR)
                .map_err(|e| e.to_string())?
                .citations()
                .len(),
            2
        );
        // The face name alone is not Oracle text: a name-only quote proves nothing.
        let name_only = Citation::OracleText {
            card: waylay,
            face: 0,
            quote: Quote::try_new("Waylay")?,
        };
        assert!(
            matches!(verdict(vec![name_only.clone()]).validate(&c, CR), Err(JudgeError::BadCitation(ref x)) if *x == name_only)
        );
        // Wrong face: the quote is from face 0 but face 1 is cited; and face 2 does not exist.
        let wrong_face = Citation::OracleText {
            card: waylay,
            face: 1,
            quote: Quote::try_new("cleanup step")?,
        };
        assert!(
            matches!(verdict(vec![wrong_face.clone()]).validate(&c, CR), Err(JudgeError::BadCitation(ref x)) if *x == wrong_face)
        );
        let no_face = Citation::OracleText {
            card: waylay,
            face: 2,
            quote: Quote::try_new("Nothing")?,
        };
        assert!(matches!(
            verdict(vec![no_face]).validate(&c, CR),
            Err(JudgeError::BadCitation(_))
        ));
        // Not a substring (the old, pre-errata wording).
        let paraphrase = Citation::OracleText {
            card: waylay,
            face: 0,
            quote: Quote::try_new("At end of turn, remove them from the game")?,
        };
        assert!(matches!(
            verdict(vec![paraphrase]).validate(&c, CR),
            Err(JudgeError::BadCitation(_))
        ));
        // Unknown card.
        let unknown = Citation::OracleText {
            card: CardId::new(Uuid::from_u128(77)),
            face: 0,
            quote: Quote::try_new("Exile them")?,
        };
        assert!(matches!(
            verdict(vec![unknown]).validate(&c, CR),
            Err(JudgeError::BadCitation(_))
        ));
        // Deserializes from the model's tagged form.
        let json = r#"{"answer":"a","confidence":"low","citations":[{"kind":"oracle_text","card":"00000000-0000-0000-0000-000000000009","face":0,"quote":"q"}],"category":"layers"}"#;
        let v: Verdict<Unvalidated> = serde_json::from_str(json)?;
        assert!(
            matches!(v.citations().first(), Some(Citation::OracleText { card, face: 0, .. }) if *card == waylay)
        );
        Ok(())
    }

    #[test]
    fn deserializes_from_model_json_and_rejects_unknown_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{"answer":"a","confidence":"low","citations":[{"kind":"rule","id":"702.15b","quote":"q"}],
                      "category":"layers"}"#;
        let v: Verdict<Unvalidated> = serde_json::from_str(json)?;
        assert_eq!(v.category(), Category::Layers);
        assert!(
            serde_json::from_str::<Verdict<Unvalidated>>(
                &json.replace("\"answer\"", "\"extra\":1,\"answer\"")
            )
            .is_err()
        );
        // A bad rule id is *not* a parse failure of the verdict: it is one
        // unreadable citation, held as data and rejected by `validate`. See
        // `one_unreadable_citation_does_not_cost_the_verdict`.
        let bad_id: Verdict<Unvalidated> = serde_json::from_str(&json.replace("702.15b", "abc"))?;
        assert!(bad_id.citations().is_empty());
        assert!(matches!(
            bad_id.validate(&ctx()?, CR),
            Err(JudgeError::MalformedCitation(_))
        ));
        // cr_version and source are not model-provided; supplying either is an unknown field.
        assert!(
            serde_json::from_str::<Verdict<Unvalidated>>(
                &json.replace("\"category\"", "\"cr_version\":\"20250801\",\"category\"")
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Verdict<Unvalidated>>(
                &json.replace("\"category\"", "\"source\":\"out_of_scope\",\"category\"")
            )
            .is_err()
        );
        Ok(())
    }

    /// The regression this file's `Citations` type exists for. A real reply
    /// (2026-09-01) carried four good rule citations behind two stubs, one of
    /// them `{"id":"","kind":"rule","quote":""}`. As a `Vec<Citation>` the
    /// empty `RuleId` failed the whole payload at the serde boundary, which
    /// `judge()` surfaces as an un-retryable `Upstream`. Per element the good
    /// four survive. Since D21 the stubs, which quote nothing, are dropped and
    /// the answer stands on the citations that are real.
    #[test]
    fn one_unreadable_citation_does_not_cost_the_verdict() -> Result<(), Box<dyn std::error::Error>>
    {
        let json = r#"{"answer":"No — it's one or the other, not both, per the cost-reduction rules.",
            "confidence":"high","category":"casting_spells","citations":[
            {"card":"0000000e-0000-0000-0000-000000000000","ruling":"0123456789abcdef","kind":"scryfall_ruling","quote":""},
            {"id":"","kind":"rule","quote":""},
            {"id":"702.15b","kind":"rule","quote":"gain that much life"}]}"#;
        let v: Verdict<Unvalidated> = serde_json::from_str(json)?;
        // The readable citation is kept and the answer survives. Both stubs
        // are unreadable: the ruling's blank quote as much as the rule's empty id.
        assert_eq!(v.citations().len(), 1, "{:?}", v.citations());
        assert!(
            matches!(v.citations().first(), Some(Citation::Rule { id, .. }) if id.as_ref() == "702.15b")
        );
        // The stubs are dropped, not held against the answer.
        let validated = v.validate(&ctx()?, CR)?;
        assert_eq!(validated.citations().len(), 1);
        // With nothing but stubs there is no answer to stand, and the first
        // one is reported as itself, not as the `NoCitations` it would look like.
        let only_stubs = json.replace("gain that much life", "x");
        let err = serde_json::from_str::<Verdict<Unvalidated>>(&only_stubs)?
            .validate(&ctx()?, CR)
            .err();
        assert!(
            matches!(err, Some(JudgeError::MalformedCitation(ref m))
                if m.error.contains("quote is empty") && m.raw.contains(r#""kind":"scryfall_ruling""#)),
            "{err:?}"
        );
        // An unreadable element that does quote something is a citation the
        // model meant, and is still rejected.
        let meant = json.replace(
            r#"{"id":"","kind":"rule","quote":""}"#,
            r#"{"id":"","kind":"rule","quote":"gain that much life"}"#,
        );
        let err = serde_json::from_str::<Verdict<Unvalidated>>(&meant)?
            .validate(&ctx()?, CR)
            .err();
        assert!(
            matches!(err, Some(JudgeError::MalformedCitation(ref m)) if m.raw.contains(r#""kind":"rule""#)),
            "{err:?}"
        );
        // So is one whose quote is under a misspelt key or is not a string:
        // only a quote that is present and quotes nothing makes a stub.
        for hidden in [
            r#"{"id":"702.15b","kind":"rule","qoute":"gain that much life"}"#,
            r#"{"id":"702.15b","kind":"rule","quote":["gain that much life"]}"#,
        ] {
            let meant = json.replace(r#"{"id":"","kind":"rule","quote":""}"#, hidden);
            let err = serde_json::from_str::<Verdict<Unvalidated>>(&meant)?
                .validate(&ctx()?, CR)
                .err();
            assert!(
                matches!(err, Some(JudgeError::MalformedCitation(_))),
                "{hidden}: {err:?}"
            );
        }
        Ok(())
    }

    /// The reply behind the 2026-09-10 Room failure cited
    /// `oracle 00000000-0000-0000-0000-000000000000#0: ""` — no card in the
    /// material, nothing quoted. That parsed, reached validation as a bad
    /// citation, and the retry notice could only say the card face was not in
    /// the material. A blank quote is unreadable, so it is a stub: dropped
    /// beside a real citation (D21), and when it is all there is, rejected
    /// with the notice that names placeholder citations.
    #[test]
    fn a_blank_quote_is_unreadable() -> Result<(), Box<dyn std::error::Error>> {
        let body = |quote: &str| {
            format!(
                r#"{{"answer":"{}","confidence":"medium","category":"multi_faced_cards","citations":[
                {{"kind":"rule","id":"702.15b","quote":"gain that much life"}},
                {{"kind":"oracle_text","card":"00000000-0000-0000-0000-000000000000","face":0,"quote":{quote}}}]}}"#,
                "x".repeat(50)
            )
        };
        for blank in [r#""""#, r#""   ""#, r#""\n\t""#] {
            let v: Verdict<Unvalidated> = serde_json::from_str(&body(blank))?;
            assert_eq!(v.citations().len(), 1, "{blank}");
            assert_eq!(v.validate(&ctx()?, CR)?.citations().len(), 1, "{blank}");
            let alone = body(blank).replace("gain that much life", "  ");
            let err = serde_json::from_str::<Verdict<Unvalidated>>(&alone)?
                .validate(&ctx()?, CR)
                .err();
            assert!(
                matches!(err, Some(JudgeError::MalformedCitation(ref m)) if m.error.contains("quote is empty")),
                "{blank}: {err:?}"
            );
        }
        // The schema the model sees is unchanged: a quote is a plain string.
        let schema = serde_json::to_value(schemars::schema_for!(Verdict<Unvalidated>))?;
        assert!(!schema.to_string().contains("Quote"), "{schema}");
        Ok(())
    }

    /// The 2026-09-20 gold runs: a ruling quoted as `"placeholder"` and one
    /// quoted as `"x"`. Both parsed, both were reported as bad citations, and
    /// the notice for those says the quote is not verbatim, which is no help
    /// to a model that had nothing to quote. They are stubs: dropped beside a
    /// real citation (D21), and named as stubs when they are all there is.
    #[test]
    fn a_placeholder_quote_is_a_stub_not_a_bad_quote() -> Result<(), Box<dyn std::error::Error>> {
        let ruling = |quote: &str| -> Result<Citation, Box<dyn std::error::Error>> {
            Ok(Citation::ScryfallRuling {
                card: CardId::new(Uuid::from_u128(7)),
                ruling: ruling_key("2020-01-01", "Lifelink is not a triggered ability."),
                quote: Quote::try_new(quote)?,
            })
        };
        for stub in [
            "placeholder",
            "\"Placeholder.\"",
            "x",
            "...",
            "N/A",
            " todo ",
        ] {
            let kept = verdict(vec![good_citation()?, ruling(stub)?]).validate(&ctx()?, CR)?;
            assert_eq!(kept.citations(), [good_citation()?], "{stub}");
            let err = verdict(vec![ruling(stub)?]).validate(&ctx()?, CR).err();
            assert!(
                matches!(err, Some(JudgeError::MalformedCitation(ref m))
                    if m.error.contains("quote") && m.raw.contains("ruling")),
                "{stub}: {err:?}"
            );
        }
        for stub in ["None.", "[TBD]"] {
            let err = verdict(vec![ruling(stub)?]).validate(&ctx()?, CR).err();
            assert!(
                matches!(err, Some(JudgeError::MalformedCitation(_))),
                "{stub}: {err:?}"
            );
        }
        // Four characters is a quote: judged against its source, not as a stub.
        let err = verdict(vec![ruling("Fear")?]).validate(&ctx()?, CR).err();
        assert!(matches!(err, Some(JudgeError::BadCitation(_))), "{err:?}");
        // A real quote, however short, is judged as a quote.
        assert!(
            verdict(vec![ruling("Lifelink is not")?])
                .validate(&ctx()?, CR)
                .is_ok()
        );
        let err = verdict(vec![ruling("Lifelink is a triggered ability")?])
            .validate(&ctx()?, CR)
            .err();
        assert!(matches!(err, Some(JudgeError::BadCitation(_))), "{err:?}");
        Ok(())
    }

    fn answering(text: &str, citations: Vec<Citation>) -> Verdict<Unvalidated> {
        Verdict::new(
            text.into(),
            Confidence::High,
            citations,
            Category::KeywordAbilities,
        )
    }

    /// Every rule number a reader sees has been quoted and checked, or the
    /// answer is sent back. The Lion's Eye Diamond answer of the 2026-09-20
    /// gold run named `605.3b` and cited only `605.1a`.
    #[test]
    fn a_rule_number_in_the_prose_must_be_among_the_citations()
    -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let uncited = |text: &str| -> Result<Vec<String>, Box<dyn std::error::Error>> {
            Ok(
                match answering(text, vec![good_citation()?]).validate(&c, CR) {
                    Err(JudgeError::UncitedRules(u)) => {
                        u.ids().iter().map(ToString::to_string).collect()
                    }
                    Ok(_) => vec![],
                    Err(e) => return Err(e.into()),
                },
            )
        };
        let pad = "Lifelink means its controller gains that much life, every time. ";
        // Cited, at the sub-rule, at its rule, and from a rule-level citation's sub-rule.
        assert!(uncited(&format!("{pad}See `702.15b`."))?.is_empty());
        assert!(uncited(&format!("{pad}See 702.15 for the keyword."))?.is_empty());
        // Named and not cited, once each, in order.
        assert_eq!(
            uncited(&format!(
                "{pad}Per `605.3b` and 117.1d (and 605.3b again), and `702.15b`."
            ))?,
            ["605.3b", "117.1d"]
        );
        // A sibling sub-rule is a different rule.
        assert_eq!(uncited(&format!("{pad}But see 702.15a."))?, ["702.15a"]);
        // Not rule numbers: money, a longer figure, a bare section, a date, a version.
        assert!(
            uncited(&format!(
                "{pad}It costs $100.50, or 1.702.15 in some notation; section 702 covers it, as of 2026.08, v1.0.0."
            ))?
            .is_empty()
        );
        Ok(())
    }

    /// Dropping a stub cannot launder a reference: if the prose leans on the
    /// dropped rule by number, the prose check rejects it.
    #[test]
    fn a_dropped_stub_does_not_cover_the_rule_it_named() -> Result<(), Box<dyn std::error::Error>> {
        let stub = Citation::Rule {
            id: RuleId::try_new("605.3b".to_owned())?,
            quote: Quote::try_new("x")?,
        };
        let text =
            "Lifelink means its controller gains that much life, and per `605.3b` it is instant.";
        let err = answering(text, vec![good_citation()?, stub.clone()])
            .validate(&ctx()?, CR)
            .err();
        assert!(
            matches!(err, Some(JudgeError::UncitedRules(ref u)) if u.ids().first().as_ref() == "605.3b"),
            "{err:?}"
        );
        // Without the reference in the prose, the stub is simply gone.
        let ok = answering(
            "Lifelink means its controller gains that much life, every single time it deals damage.",
            vec![good_citation()?, stub],
        )
        .validate(&ctx()?, CR)?;
        assert_eq!(ok.citations(), [good_citation()?]);
        Ok(())
    }

    /// A bad citation is reported before an uncited number: the citation may
    /// be the very one that would have covered it.
    #[test]
    fn citations_are_judged_before_the_prose() -> Result<(), Box<dyn std::error::Error>> {
        let bad = Citation::Rule {
            id: RuleId::try_new("605.3b".to_owned())?,
            quote: Quote::try_new("not in the material")?,
        };
        let v = answering(
            "Per `605.3b`, a mana ability resolves at once, without using the stack at all.",
            vec![bad],
        );
        assert!(matches!(
            v.validate(&ctx()?, CR),
            Err(JudgeError::BadCitation(_))
        ));
        Ok(())
    }

    /// The Waylay failure behind those stubs: the card had no rulings in the
    /// material, and the model filed its Oracle text under an invented ruling
    /// key. That is detected, for the notice, and never repaired.
    #[test]
    fn oracle_text_filed_as_a_ruling_is_recognised_and_still_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let misfiled =
            |card: CardId, quote: &str| -> Result<Citation, Box<dyn std::error::Error>> {
                Ok(Citation::ScryfallRuling {
                    card,
                    ruling: "0000000000000000".parse()?,
                    quote: Quote::try_new(quote)?,
                })
            };
        let cleanup = "Exile them at the beginning of the next cleanup step.";
        assert_eq!(
            misfiled_oracle_text(&misfiled(waylay_id(), cleanup)?, &c),
            Some(0)
        );
        assert_eq!(
            misfiled_oracle_text(&misfiled(waylay_id(), "Nothing here.")?, &c),
            Some(1)
        );
        // Oracle text only: the type line is not something a face quotes.
        assert_eq!(
            misfiled_oracle_text(&misfiled(waylay_id(), "Instant")?, &c),
            None
        );
        // Not this card's text, not a card in the material, not a ruling at all.
        assert_eq!(
            misfiled_oracle_text(&misfiled(waylay_id(), "Draw a card now.")?, &c),
            None
        );
        assert_eq!(
            misfiled_oracle_text(&misfiled(CardId::new(Uuid::from_u128(99)), cleanup)?, &c),
            None
        );
        assert_eq!(misfiled_oracle_text(&good_citation()?, &c), None);
        // A ruling that exists is not second-guessed, whatever it quotes.
        let real = Citation::ScryfallRuling {
            card: CardId::new(Uuid::from_u128(7)),
            ruling: ruling_key("2020-01-01", "Lifelink is not a triggered ability."),
            quote: Quote::try_new(cleanup)?,
        };
        assert_eq!(misfiled_oracle_text(&real, &c), None);
        // Recognised is not admitted.
        let err = verdict(vec![misfiled(waylay_id(), cleanup)?])
            .validate(&c, CR)
            .err();
        assert!(matches!(err, Some(JudgeError::BadCitation(_))), "{err:?}");
        Ok(())
    }

    #[test]
    fn malformed_citations_are_retryable_and_never_reach_validated()
    -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let body = |cits: &str| {
            format!(
                r#"{{"answer":"{}","confidence":"low","category":"layers","citations":[{cits}]}}"#,
                "x".repeat(50)
            )
        };
        // Unknown kind, missing field, bad uuid: each is data, not a parse error.
        for bad in [
            r#"{"kind":"vibes","quote":"q"}"#,
            r#"{"kind":"rule","quote":"q"}"#,
            r#"{"kind":"oracle_text","card":"not-a-uuid","face":0,"quote":"q"}"#,
            r#"{"kind":"rule","id":"702.19B","quote":"q"}"#,
        ] {
            let v: Verdict<Unvalidated> = serde_json::from_str(&body(bad))?;
            assert!(v.citations().is_empty(), "{bad}");
            assert!(
                matches!(v.validate(&c, CR), Err(JudgeError::MalformedCitation(_))),
                "{bad}"
            );
        }
        // A malformed element outranks the NoCitations it causes: it is the
        // more specific thing to tell the model on the retry.
        let only_bad: Verdict<Unvalidated> =
            serde_json::from_str(&body(r#"{"kind":"vibes","quote":"q"}"#))?;
        assert!(matches!(
            only_bad.validate(&c, CR),
            Err(JudgeError::MalformedCitation(_))
        ));
        // A verdict whose citations all parsed is unaffected.
        let good: Verdict<Unvalidated> = serde_json::from_str(&body(
            r#"{"kind":"rule","id":"702.15b","quote":"gain that much life"}"#,
        ))?;
        assert_eq!(good.validate(&c, CR)?.citations().len(), 1);
        // Still a hard error if `citations` is not an array at all: that is a
        // schema violation, not one bad element.
        assert!(
            serde_json::from_str::<Verdict<Unvalidated>>(&body("").replace("[]", "\"nope\""))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn malformed_raw_is_bounded_on_a_char_boundary() -> Result<(), Box<dyn std::error::Error>> {
        // The CR's curly apostrophes make byte-truncation a panic risk.
        let quote = "doesn’t require mana of that type — ".repeat(40);
        let json = format!(
            r#"{{"answer":"{}","confidence":"low","category":"layers","citations":[{{"kind":"rule","id":"","quote":"{quote}"}}]}}"#,
            "x".repeat(50)
        );
        let v: Verdict<Unvalidated> = serde_json::from_str(&json)?;
        let Some(JudgeError::MalformedCitation(m)) = v.validate(&ctx()?, CR).err() else {
            return Err("expected MalformedCitation".into());
        };
        assert_eq!(
            m.raw.chars().count(),
            crate::MALFORMED_RAW_CHARS + 1,
            "truncated plus the ellipsis"
        );
        assert!(m.raw.ends_with('…'));
        Ok(())
    }

    #[test]
    fn citations_round_trip_as_a_bare_array() -> Result<(), Box<dyn std::error::Error>> {
        // Serialization must be unchanged: persisted verdicts are `Validated`,
        // and the stored shape is a plain array of citations.
        let ok = verdict(vec![good_citation()?]).validate(&ctx()?, CR)?;
        let v = serde_json::to_value(&ok)?;
        let arr = v
            .get("citations")
            .and_then(|c| c.as_array())
            .ok_or("citations is not an array")?;
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr.first()
                .and_then(|c| c.get("kind"))
                .and_then(|k| k.as_str()),
            Some("rule")
        );
        Ok(())
    }

    #[test]
    fn validated_serializes_with_cr_version() -> Result<(), Box<dyn std::error::Error>> {
        let ok = verdict(vec![good_citation()?]).validate(&ctx()?, CR)?;
        let v = serde_json::to_value(&ok)?;
        assert_eq!(
            v.get("cr_version").and_then(|x| x.as_str()),
            Some("20250801")
        );
        assert_eq!(v.get("source").and_then(|x| x.as_str()), Some("cr"));
        assert_eq!(v.get("answer").and_then(|x| x.as_str()), Some(ANSWER));
        // Unvalidated serializes without them.
        let u = serde_json::to_value(verdict(vec![]))?;
        assert!(u.get("cr_version").is_none() && u.get("source").is_none());
        Ok(())
    }

    #[test]
    fn schema_has_no_cr_version_or_source() {
        let s = schemars::schema_for!(Verdict).to_value();
        let props = s.get("properties").and_then(|p| p.as_object());
        assert!(
            props.is_some_and(|p| !p.contains_key("cr_version")
                && !p.contains_key("source")
                && p.contains_key("citations")),
            "{s}"
        );
        assert_eq!(s.get("title").and_then(|t| t.as_str()), Some("Verdict"));
    }
}
