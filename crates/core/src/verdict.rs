//! `Verdict<Unvalidated | Validated>` — only a validated verdict can be
//! persisted or rendered (invariant I3). The only way to obtain
//! `Verdict<Validated>` is `Verdict::<Unvalidated>::validate`.
//!
//! Enforced by construction:
//! * `Deserialize` is implemented for `Verdict<Unvalidated>` **only**, so
//!   `serde_json::from_str::<Verdict<Validated>>(..)` does not compile.
//! * `Validated` carries the `cr_version` stamped from Context and has a
//!   private field, so no code outside `validate` can build one.
//! * The model never reports `cr_version` or `source`: neither is in the
//!   model-facing schema. `cr_version` is taken from the retrieved CR chunks
//!   and `source` from the extraction, as an [`AnswerableSource`], so a
//!   verdict cannot claim to be out of scope to dodge the citation check.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{AnswerableSource, Category, Citation, Confidence, Context, CrVersion, EmptyVerdict, JudgeError, Source};

/// An answer shorter than this (in characters, trimmed) is not an answer:
/// `"pending"`, `"see above"` and the like fail validation as
/// [`EmptyVerdict::ShortAnswer`].
pub const MIN_ANSWER_CHARS: usize = 40;

mod sealed {
    pub trait Sealed {}
}

/// Validation state marker. Sealed: only `Unvalidated` and `Validated` exist.
pub trait State: sealed::Sealed + Clone + core::fmt::Debug + PartialEq + Serialize + Send + Sync + 'static {}

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
}

impl sealed::Sealed for Unvalidated {}
impl sealed::Sealed for Validated {}
impl State for Unvalidated {}
impl State for Validated {}

/// The model-facing shape: what the synthesizer's structured output must match.
/// Private so that `Deserialize` can only be reached through `Verdict<Unvalidated>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Verdict")]
struct VerdictData {
    /// The answer, in plain prose, quoting current Oracle text where relevant.
    answer: String,
    /// Self-assessed confidence.
    confidence: Confidence,
    /// Each citation quotes a span verbatim from a rule, ruling or prior call in Context.
    citations: Vec<Citation>,
    /// The category that best fits the question.
    category: Category,
}

/// The synthesizer's structured answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(bound = "S: State")]
pub struct Verdict<S: State = Unvalidated> {
    #[serde(flatten)]
    data: VerdictData,
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
    /// Citations, in the model's order.
    #[must_use]
    pub fn citations(&self) -> &[Citation] {
        &self.data.citations
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
    pub fn new(answer: String, confidence: Confidence, citations: Vec<Citation>, category: Category) -> Self {
        Self { data: VerdictData { answer, confidence, citations, category }, state: Unvalidated {} }
    }

    /// Check the verdict against `ctx`:
    /// (0) the answer is at least [`MIN_ANSWER_CHARS`] long and cites
    ///     something at all;
    /// (a) each referenced rule / ruling / prior call / card face exists in
    ///     Context, and
    /// (b) each quote is a non-empty verbatim substring of that source (for
    ///     Oracle text: the face's current text or its name);
    /// then stamp the CR version of the retrieved chunks and `source`, which
    /// is the extraction's classification (only answerable sources reach
    /// synthesis, and the type says so).
    ///
    /// # Errors
    /// `JudgeError::EmptyVerdict` for (0), `JudgeError::BadCitation` carrying
    /// the first offending citation for (a)/(b), or `JudgeError::Upstream` if
    /// Context holds no CR chunks (the category map always injects some, so
    /// this indicates a broken retriever).
    pub fn validate(self, ctx: &Context, source: AnswerableSource) -> Result<Verdict<Validated>, JudgeError> {
        if let Some(e) = self.emptiness() {
            return Err(JudgeError::EmptyVerdict(e));
        }
        for c in &self.data.citations {
            if !citation_ok(c, ctx) {
                return Err(JudgeError::BadCitation(c.clone()));
            }
        }
        let cr_version = ctx
            .cr_version()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("context holds no CR chunks; cannot stamp cr_version"))?;
        Ok(Verdict { data: self.data, state: Validated { cr_version, source } })
    }
}

impl Verdict<Unvalidated> {
    /// Why this verdict counts as empty, if it does (check (0) of `validate`).
    fn emptiness(&self) -> Option<EmptyVerdict> {
        let chars = self.data.answer.trim().chars().count();
        if chars < MIN_ANSWER_CHARS {
            return Some(EmptyVerdict::ShortAnswer { chars });
        }
        if self.data.citations.is_empty() {
            return Some(EmptyVerdict::NoCitations);
        }
        None
    }
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
}

/// Only the unvalidated state can be deserialized: model output and stored
/// JSON both come back as `Verdict<Unvalidated>` and must pass `validate`.
impl<'de> Deserialize<'de> for Verdict<Unvalidated> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        VerdictData::deserialize(d).map(|data| Verdict { data, state: Unvalidated {} })
    }
}

/// The model-facing schema (used for `output_config.format`).
impl JsonSchema for Verdict<Unvalidated> {
    fn schema_name() -> Cow<'static, str> {
        VerdictData::schema_name()
    }
    fn schema_id() -> Cow<'static, str> {
        VerdictData::schema_id()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        VerdictData::json_schema(g)
    }
    fn inline_schema() -> bool {
        VerdictData::inline_schema()
    }
}

fn citation_ok(c: &Citation, ctx: &Context) -> bool {
    let quote = c.quote().trim();
    if quote.is_empty() {
        return false;
    }
    match c {
        Citation::Rule { id, .. } => ctx.rule(id).is_some_and(|r| r.contains_quote(quote)),
        Citation::ScryfallRuling { card, idx, .. } => {
            ctx.ruling(*card, *idx).is_some_and(|r| r.text.contains(quote))
        }
        Citation::PriorCall { id, .. } => ctx.prior_call(*id).is_some_and(|p| p.answer.contains(quote)),
        Citation::OracleText { card, face, .. } => {
            ctx.card(*card).and_then(|c| c.face(*face)).is_some_and(|f| f.contains_quote(quote))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallId, Card, CardId, Face, Layout, RuleChunk, RuleId, Ruling};
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
                face("Waylay", "Create three 2/2 white Knight creature tokens. Exile them at the beginning of the next cleanup step."),
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
            rules: vec![rule("702.15b", "Damage dealt by a source with lifelink causes that source's controller to gain that much life.")?],
            rulings: vec![Ruling { card, idx: 0, published_at: "2020-01-01".into(), text: "Lifelink is not a triggered ability.".into() }],
            ..Context::default()
        })
    }

    const ANSWER: &str = "You gain life simultaneously with the damage being dealt.";

    fn verdict(citations: Vec<Citation>) -> Verdict<Unvalidated> {
        Verdict::new(ANSWER.into(), Confidence::High, citations, Category::KeywordAbilities)
    }

    const CR: AnswerableSource = AnswerableSource::Cr;

    fn good_citation() -> Result<Citation, Box<dyn std::error::Error>> {
        Ok(Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "gain that much life".into() })
    }

    #[test]
    fn accepts_valid_citations_and_stamps_cr_version() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let v = verdict(vec![
            Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "gain that much life".into() },
            Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "Example: something.".into() },
            Citation::ScryfallRuling { card: CardId::new(Uuid::from_u128(7)), idx: 0, quote: "not a triggered ability".into() },
        ]);
        let ok = v.validate(&c, CR).map_err(|e| e.to_string())?;
        assert_eq!(ok.citations().len(), 3);
        assert_eq!(ok.confidence(), Confidence::High);
        assert_eq!(ok.cr_version().as_ref(), "20250801");
        assert_eq!(ok.source(), Source::Cr);
        Ok(())
    }

    #[test]
    fn rejects_reference_not_in_context() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let bad = Citation::Rule { id: RuleId::try_new("702.19".to_owned())?, quote: "gain that much life".into() };
        let err = verdict(vec![bad.clone()]).validate(&c, CR).err();
        assert!(matches!(err, Some(JudgeError::BadCitation(ref x)) if *x == bad), "{err:?}");

        let bad_ruling = Citation::ScryfallRuling { card: CardId::new(Uuid::from_u128(7)), idx: 9, quote: "Lifelink".into() };
        assert!(matches!(verdict(vec![bad_ruling]).validate(&c, CR), Err(JudgeError::BadCitation(_))));

        let bad_prior = Citation::PriorCall { id: CallId::new(Uuid::from_u128(1)), quote: "x".into() };
        assert!(matches!(verdict(vec![bad_prior]).validate(&c, CR), Err(JudgeError::BadCitation(_))));
        Ok(())
    }

    #[test]
    fn rejects_quote_not_substring() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let bad = Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "gain twice that much life".into() };
        assert!(matches!(verdict(vec![bad]).validate(&c, CR), Err(JudgeError::BadCitation(_))));
        let empty = Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "   ".into() };
        assert!(matches!(verdict(vec![empty]).validate(&c, CR), Err(JudgeError::BadCitation(_))));
        Ok(())
    }

    #[test]
    fn rejects_context_without_rules() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(verdict(vec![good_citation()?]).validate(&Context::default(), CR), Err(JudgeError::BadCitation(_))));
        // Nothing to cite and nothing to stamp: the empty-citations check comes first.
        assert!(matches!(verdict(vec![]).validate(&Context::default(), CR), Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))));
        Ok(())
    }

    #[test]
    fn rejects_empty_verdicts() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        // No citations for a CR answer.
        assert!(matches!(verdict(vec![]).validate(&c, CR), Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))));
        // No citations for a Commander answer.
        let cmd = Verdict::new(ANSWER.into(), Confidence::High, vec![], Category::Commander);
        assert!(matches!(cmd.validate(&c, AnswerableSource::Commander), Err(JudgeError::EmptyVerdict(EmptyVerdict::NoCitations))));
        // A "pending" answer, even with a valid citation.
        let short = Verdict::new("pending".into(), Confidence::High, vec![good_citation()?], Category::KeywordAbilities);
        assert!(matches!(short.validate(&c, CR), Err(JudgeError::EmptyVerdict(EmptyVerdict::ShortAnswer { chars: 7 }))));
        // Whitespace does not count.
        let blank = Verdict::new(" ".repeat(50), Confidence::High, vec![good_citation()?], Category::KeywordAbilities);
        assert!(matches!(blank.validate(&c, CR), Err(JudgeError::EmptyVerdict(EmptyVerdict::ShortAnswer { chars: 0 }))));
        // Exactly the minimum passes.
        let exact = Verdict::new("x".repeat(MIN_ANSWER_CHARS), Confidence::High, vec![good_citation()?], Category::KeywordAbilities);
        assert!(exact.validate(&c, CR).is_ok());
        // Commander verdicts carry their source through validation.
        let cmd = Verdict::new(ANSWER.into(), Confidence::High, vec![good_citation()?], Category::Commander);
        assert_eq!(cmd.validate(&c, AnswerableSource::Commander)?.source(), Source::Commander);
        Ok(())
    }

    #[test]
    fn oracle_text_citations() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let waylay = waylay_id();
        // Valid: a substring of face 0's Oracle text; of face 1's.
        let ok = verdict(vec![
            Citation::OracleText { card: waylay, face: 0, quote: "Exile them at the beginning of the next cleanup step.".into() },
            Citation::OracleText { card: waylay, face: 1, quote: "Nothing here".into() },
        ]);
        assert_eq!(ok.validate(&c, CR).map_err(|e| e.to_string())?.citations().len(), 2);
        // The face name alone is not Oracle text: a name-only quote proves nothing.
        let name_only = Citation::OracleText { card: waylay, face: 0, quote: "Waylay".into() };
        assert!(matches!(verdict(vec![name_only.clone()]).validate(&c, CR), Err(JudgeError::BadCitation(ref x)) if *x == name_only));
        // Wrong face: the quote is from face 0 but face 1 is cited; and face 2 does not exist.
        let wrong_face = Citation::OracleText { card: waylay, face: 1, quote: "cleanup step".into() };
        assert!(matches!(verdict(vec![wrong_face.clone()]).validate(&c, CR), Err(JudgeError::BadCitation(ref x)) if *x == wrong_face));
        let no_face = Citation::OracleText { card: waylay, face: 2, quote: "Nothing".into() };
        assert!(matches!(verdict(vec![no_face]).validate(&c, CR), Err(JudgeError::BadCitation(_))));
        // Not a substring (the old, pre-errata wording).
        let paraphrase = Citation::OracleText { card: waylay, face: 0, quote: "At end of turn, remove them from the game".into() };
        assert!(matches!(verdict(vec![paraphrase]).validate(&c, CR), Err(JudgeError::BadCitation(_))));
        // Unknown card.
        let unknown = Citation::OracleText { card: CardId::new(Uuid::from_u128(77)), face: 0, quote: "Exile them".into() };
        assert!(matches!(verdict(vec![unknown]).validate(&c, CR), Err(JudgeError::BadCitation(_))));
        // Deserializes from the model's tagged form.
        let json = r#"{"answer":"a","confidence":"low","citations":[{"kind":"oracle_text","card":"00000000-0000-0000-0000-000000000009","face":0,"quote":"q"}],"category":"layers"}"#;
        let v: Verdict<Unvalidated> = serde_json::from_str(json)?;
        assert!(matches!(v.citations().first(), Some(Citation::OracleText { card, face: 0, .. }) if *card == waylay));
        Ok(())
    }

    #[test]
    fn deserializes_from_model_json_and_rejects_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{"answer":"a","confidence":"low","citations":[{"kind":"rule","id":"702.15b","quote":"q"}],
                      "category":"layers"}"#;
        let v: Verdict<Unvalidated> = serde_json::from_str(json)?;
        assert_eq!(v.category(), Category::Layers);
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("\"answer\"", "\"extra\":1,\"answer\"")).is_err());
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("702.15b", "abc")).is_err());
        // cr_version and source are not model-provided; supplying either is an unknown field.
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("\"category\"", "\"cr_version\":\"20250801\",\"category\"")).is_err());
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("\"category\"", "\"source\":\"out_of_scope\",\"category\"")).is_err());
        Ok(())
    }

    #[test]
    fn validated_serializes_with_cr_version() -> Result<(), Box<dyn std::error::Error>> {
        let ok = verdict(vec![good_citation()?]).validate(&ctx()?, CR)?;
        let v = serde_json::to_value(&ok)?;
        assert_eq!(v.get("cr_version").and_then(|x| x.as_str()), Some("20250801"));
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
            props.is_some_and(|p| !p.contains_key("cr_version") && !p.contains_key("source") && p.contains_key("citations")),
            "{s}"
        );
        assert_eq!(s.get("title").and_then(|t| t.as_str()), Some("Verdict"));
    }

}
