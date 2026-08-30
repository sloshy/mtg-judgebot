//! `Verdict<Unvalidated | Validated>` — only a validated verdict can be
//! persisted or rendered (invariant I3). The only way to obtain
//! `Verdict<Validated>` is `Verdict::<Unvalidated>::validate`.
//!
//! Enforced by construction:
//! * `Deserialize` is implemented for `Verdict<Unvalidated>` **only**, so
//!   `serde_json::from_str::<Verdict<Validated>>(..)` does not compile.
//! * `Validated` carries the `cr_version` stamped from Context and has a
//!   private field, so no code outside `validate` can build one.
//! * The model never reports `cr_version`: it is not in the model-facing
//!   schema, and is taken from the retrieved CR chunks instead.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{Category, Citation, Confidence, Context, CrVersion, JudgeError, Source};

mod sealed {
    pub trait Sealed {}
}

/// Validation state marker. Sealed: only `Unvalidated` and `Validated` exist.
pub trait State: sealed::Sealed + Clone + core::fmt::Debug + PartialEq + Serialize + Send + Sync + 'static {}

/// Fresh from the model; citations not yet checked against Context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Unvalidated {}

/// Every citation exists in Context and every quote is a substring of its
/// source. Carries the CR version the context was retrieved under.
/// Deliberately **not** `Deserialize` and not constructible outside `validate`:
///
/// ```compile_fail
/// let _: judge_core::Verdict<judge_core::Validated> = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Validated {
    cr_version: CrVersion,
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
    /// Which rules body the answer draws on.
    source: Source,
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
    /// Rules body the answer draws on.
    #[must_use]
    pub fn source(&self) -> Source {
        self.data.source
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
        source: Source,
    ) -> Self {
        Self { data: VerdictData { answer, confidence, citations, category, source }, state: Unvalidated {} }
    }

    /// Check every citation against `ctx`:
    /// (a) the referenced rule / ruling / prior call exists in Context, and
    /// (b) the quote is a non-empty verbatim substring of that source;
    /// then stamp the CR version of the retrieved chunks.
    ///
    /// # Errors
    /// `JudgeError::BadCitation` carrying the first offending citation, or
    /// `JudgeError::Upstream` if Context holds no CR chunks (the category map
    /// always injects some, so this indicates a broken retriever).
    pub fn validate(self, ctx: &Context) -> Result<Verdict<Validated>, JudgeError> {
        for c in &self.data.citations {
            if !citation_ok(c, ctx) {
                return Err(JudgeError::BadCitation(c.clone()));
            }
        }
        let cr_version = ctx
            .cr_version()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("context holds no CR chunks; cannot stamp cr_version"))?;
        Ok(Verdict { data: self.data, state: Validated { cr_version } })
    }
}

impl Verdict<Validated> {
    /// CR version the verdict was validated against (from the retrieved chunks).
    #[must_use]
    pub fn cr_version(&self) -> &CrVersion {
        &self.state.cr_version
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallId, CardId, RuleChunk, RuleId, Ruling};
    use uuid::Uuid;

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
            rules: vec![rule("702.15b", "Damage dealt by a source with lifelink causes that source's controller to gain that much life.")?],
            rulings: vec![Ruling { card, idx: 0, published_at: "2020-01-01".into(), text: "Lifelink is not a triggered ability.".into() }],
            ..Context::default()
        })
    }

    fn verdict(citations: Vec<Citation>) -> Verdict<Unvalidated> {
        Verdict::new(
            "You gain life simultaneously.".into(),
            Confidence::High,
            citations,
            Category::KeywordAbilities,
            Source::Cr,
        )
    }

    #[test]
    fn accepts_valid_citations_and_stamps_cr_version() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let v = verdict(vec![
            Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "gain that much life".into() },
            Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "Example: something.".into() },
            Citation::ScryfallRuling { card: CardId::new(Uuid::from_u128(7)), idx: 0, quote: "not a triggered ability".into() },
        ]);
        let ok = v.validate(&c).map_err(|e| e.to_string())?;
        assert_eq!(ok.citations().len(), 3);
        assert_eq!(ok.confidence(), Confidence::High);
        assert_eq!(ok.cr_version().as_ref(), "20250801");
        Ok(())
    }

    #[test]
    fn rejects_reference_not_in_context() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let bad = Citation::Rule { id: RuleId::try_new("702.19".to_owned())?, quote: "gain that much life".into() };
        let err = verdict(vec![bad.clone()]).validate(&c).err();
        assert!(matches!(err, Some(JudgeError::BadCitation(ref x)) if *x == bad), "{err:?}");

        let bad_ruling = Citation::ScryfallRuling { card: CardId::new(Uuid::from_u128(7)), idx: 9, quote: "Lifelink".into() };
        assert!(matches!(verdict(vec![bad_ruling]).validate(&c), Err(JudgeError::BadCitation(_))));

        let bad_prior = Citation::PriorCall { id: CallId::new(Uuid::from_u128(1)), quote: "x".into() };
        assert!(matches!(verdict(vec![bad_prior]).validate(&c), Err(JudgeError::BadCitation(_))));
        Ok(())
    }

    #[test]
    fn rejects_quote_not_substring() -> Result<(), Box<dyn std::error::Error>> {
        let c = ctx()?;
        let bad = Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "gain twice that much life".into() };
        assert!(matches!(verdict(vec![bad]).validate(&c), Err(JudgeError::BadCitation(_))));
        let empty = Citation::Rule { id: RuleId::try_new("702.15b".to_owned())?, quote: "   ".into() };
        assert!(matches!(verdict(vec![empty]).validate(&c), Err(JudgeError::BadCitation(_))));
        Ok(())
    }

    #[test]
    fn rejects_context_without_rules() {
        assert!(matches!(verdict(vec![]).validate(&Context::default()), Err(JudgeError::Upstream(_))));
    }

    #[test]
    fn deserializes_from_model_json_and_rejects_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{"answer":"a","confidence":"low","citations":[{"kind":"rule","id":"702.15b","quote":"q"}],
                      "category":"layers","source":"cr"}"#;
        let v: Verdict<Unvalidated> = serde_json::from_str(json)?;
        assert_eq!(v.category(), Category::Layers);
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("\"answer\"", "\"extra\":1,\"answer\"")).is_err());
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("702.15b", "abc")).is_err());
        // cr_version is no longer model-provided; supplying it is an unknown field.
        assert!(serde_json::from_str::<Verdict<Unvalidated>>(&json.replace("\"source\":\"cr\"", "\"source\":\"cr\",\"cr_version\":\"20250801\"")).is_err());
        Ok(())
    }

    #[test]
    fn validated_serializes_with_cr_version() -> Result<(), Box<dyn std::error::Error>> {
        let ok = verdict(vec![]).validate(&ctx()?)?;
        let v = serde_json::to_value(&ok)?;
        assert_eq!(v.get("cr_version").and_then(|x| x.as_str()), Some("20250801"));
        assert_eq!(v.get("answer").and_then(|x| x.as_str()), Some("You gain life simultaneously."));
        // Unvalidated serializes without it.
        let u = serde_json::to_value(verdict(vec![]))?;
        assert!(u.get("cr_version").is_none());
        Ok(())
    }

    #[test]
    fn schema_has_no_cr_version() {
        let s = schemars::schema_for!(Verdict).to_value();
        let props = s.get("properties").and_then(|p| p.as_object());
        assert!(props.is_some_and(|p| !p.contains_key("cr_version") && p.contains_key("citations")), "{s}");
        assert_eq!(s.get("title").and_then(|t| t.as_str()), Some("Verdict"));
    }

}
