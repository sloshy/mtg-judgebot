//! Typed button `custom_id`s. Discord hands a button press back as an opaque
//! string; [`ButtonAction`] is the only way one is built or read, so a click
//! either parses into an exhaustive enum or is rejected, never guessed at.
//!
//! Wire shapes, all well under Discord's 100-character limit (the longest
//! possible English `card` id is 89 characters, a test holds it, and a
//! locale suffix adds a few):
//!
//! * `rate:<call uuid>:<1|2|3>`
//! * `card:<asker>:<c|p>:<start>-<end>|_:<digest>:<oracle id>[:<locale>]`: pick that
//!   card for the ambiguous span at bytes `start..end` of the question (`_`:
//!   the span was not found in it, so the pick appends the name). The asker
//!   is a Discord user id, `c`/`p` the [`Audience`], the digest 16 hex digits
//!   of the prompt's content ([`Digest`]), the oracle id 32 hex digits. The
//!   question itself is not here: it is read back from the message the button
//!   is on ([`super::render::PickPrompt`]), whose content must match the digest.
//!   A last `:<locale>` (`judge_core::Locale::id`) names the language the
//!   prompt is written in, so it is parsed with that language's template and
//!   the answer is in the asker's language. English is written with none, the
//!   shape every earlier build reads and sends.
//! * `retry:<asker>:<c|p>:<digest>[:<locale>]`: ask the question on the
//!   button's message again, after a maintenance reply
//!   ([`super::render::RetryPrompt`], whose content must match the digest),
//!   in the language its locale names.
//! * `pick:<uuid>:<n>`: the shape a pick had while picks were held in memory.
//!   It parses, to [`ButtonAction::LegacyPick`], so a button sent before the
//!   upgrade is answered as expired rather than as unrecognised.
//! * `rate-replace:<call uuid>:<1|2|3>` and `rate-keep:<call uuid>`: the
//!   answer to "you already rated this; replace it?" (D38).
//! * `rate-cat:<call uuid>:<1|2>`: the category menu a gateway-delivered
//!   rating shows before its form ([`super::feedback`]).
//!
//! A rating's form (a modal) has an id of its own, [`FormId`]:
//! `rate-form:<call uuid>:<1|2>[:<category>]`, the category present when it
//! was chosen before the form opened. Each step carries the call and the
//! score, so any process answers any step, after any restart. The rater's
//! language is not carried: every interaction names its user's own.

use std::{fmt, num::NonZeroU64, ops::Range, str::FromStr};

use judge_core::{CallId, CardId, Fault, FeedbackCategory, Locale, Score};
use uuid::Uuid;

use super::{Audience, pick::Digest};

/// Discord's limit on a component `custom_id`, in characters.
pub const CUSTOM_ID_LIMIT: usize = 100;

const RATE: &str = "rate";
const REPLACE: &str = "rate-replace";
const KEEP: &str = "rate-keep";
const CATEGORY: &str = "rate-cat";
const FORM: &str = "rate-form";
const CARD: &str = "card";
const RETRY: &str = "retry";
const LEGACY_PICK: &str = "pick";
/// The span marker for "not found in the question".
const NOT_FOUND: &str = "_";

/// What a button press means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ButtonAction {
    /// Rate the persisted call `call` with `score`.
    Rate {
        /// The call being rated.
        call: CallId,
        /// The rating.
        score: Score,
    },
    /// Pick a card for the first ambiguous span of the question on the
    /// button's message.
    Pick(Pick),
    /// Ask the question on the button's message again: it was answered
    /// with the maintenance reply.
    Retry(Retry),
    /// A pick button from a release that held the question in memory
    /// (`pick:<uuid>:<n>`). Nothing can answer it any more.
    LegacyPick,
    /// Replace the rater's earlier rating of `call` with `score`.
    Replace {
        /// The call being rated.
        call: CallId,
        /// The new rating.
        score: Score,
    },
    /// Keep the rater's earlier rating of `call`.
    Keep {
        /// The call.
        call: CallId,
    },
    /// The category menu of a rating that finds fault (a string select;
    /// the chosen category is the interaction's value, not in the id).
    Category {
        /// The call being rated.
        call: CallId,
        /// The fault being given a reason.
        fault: Fault,
    },
}

/// A rating form's (modal's) `custom_id`: the call, the fault, and the
/// category when it was chosen before the form opened (the form then has
/// no category menu of its own).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FormId {
    /// The call being rated.
    pub call: CallId,
    /// The fault being given a reason.
    pub fault: Fault,
    /// The category, when chosen beforehand.
    pub category: Option<FeedbackCategory>,
}

impl FormId {
    /// The `custom_id` to put on the form. At most 65 characters.
    #[must_use]
    pub fn to_custom_id(&self) -> String {
        self.to_string()
    }

    /// Parse a form's `custom_id` back.
    ///
    /// # Errors
    /// Anything but `rate-form:<call uuid>:<1|2>[:<category>]`.
    pub fn parse(custom_id: &str) -> Result<Self, ParseError> {
        custom_id.parse()
    }
}

impl fmt::Display for FormId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{FORM}:{}:{}", self.call, fault_digit(self.fault))?;
        match self.category {
            Some(c) => write!(f, ":{}", c.id()),
            None => Ok(()),
        }
    }
}

impl FromStr for FormId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let len = s.chars().count();
        if len > CUSTOM_ID_LIMIT {
            return Err(ParseError::TooLong(len));
        }
        let (kind, rest) = s.split_once(':').ok_or(ParseError::Shape)?;
        if kind != FORM {
            return Err(ParseError::UnknownKind(kind.to_owned()));
        }
        let mut parts = rest.split(':');
        let (Some(call), Some(fault), category, None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ParseError::Shape);
        };
        Ok(Self {
            call: CallId::new(uuid(call)?),
            fault: parse_fault(fault)?,
            category: category
                .map(|c| {
                    FeedbackCategory::parse(c).ok_or_else(|| ParseError::BadCategory(c.to_owned()))
                })
                .transpose()?,
        })
    }
}

const fn fault_digit(fault: Fault) -> u8 {
    fault.score() as u8
}

fn parse_score(n: &str) -> Result<Score, ParseError> {
    match n {
        "1" => Ok(Score::Incorrect),
        "2" => Ok(Score::Partial),
        "3" => Ok(Score::Correct),
        other => Err(ParseError::BadScore(other.to_owned())),
    }
}

fn parse_fault(n: &str) -> Result<Fault, ParseError> {
    match n {
        "1" => Ok(Fault::Incorrect),
        "2" => Ok(Fault::Partial),
        other => Err(ParseError::BadScore(other.to_owned())),
    }
}

/// `<call uuid>:<digit>`, the digit read by `digit`.
fn call_and<T>(
    rest: &str,
    digit: impl Fn(&str) -> Result<T, ParseError>,
) -> Result<(CallId, T), ParseError> {
    let (id, n) = rest.split_once(':').ok_or(ParseError::Shape)?;
    Ok((CallId::new(uuid(id)?), digit(n)?))
}

/// A maintenance reply's Retry button. The question is on the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Retry {
    /// Discord user id of the asker; only they may retry.
    pub asker: NonZeroU64,
    /// Who the answer is for.
    pub audience: Audience,
    /// The content of the reply this button was made for.
    pub digest: Digest,
    /// The language the reply is written in.
    pub locale: Locale,
}

/// One "did you mean…?" button: everything a click needs except the
/// question, which the message it sits on carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pick {
    /// Discord user id of the asker; only they may pick.
    pub asker: NonZeroU64,
    /// Who the answer is for; the pick re-runs the question for them.
    pub audience: Audience,
    /// Where the ambiguous span is in the question, or `None` when it is not
    /// in it (the pick then appends the name, as `question::pin_card` does).
    pub span: Option<ByteSpan>,
    /// The content of the prompt this button was made for.
    pub digest: Digest,
    /// The card this button picks.
    pub card: CardId,
    /// The language the prompt is written in, and the answer will be.
    pub locale: Locale,
}

/// A non-empty byte range of a question. `u16` bounds hold any question
/// `/judge` accepts (its `max_length` in characters is at most a quarter of
/// `u16::MAX` in bytes), and keep the `custom_id` short.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ByteSpan {
    start: u16,
    end: u16,
}

impl ByteSpan {
    /// `start..end`, or `None` when it is empty or does not fit in `u16`.
    #[must_use]
    pub fn new(r: Range<usize>) -> Option<Self> {
        let start = u16::try_from(r.start).ok()?;
        let end = u16::try_from(r.end).ok()?;
        (start < end).then_some(Self { start, end })
    }

    /// The range, to index the question with.
    #[must_use]
    pub fn range(self) -> Range<usize> {
        usize::from(self.start)..usize::from(self.end)
    }
}

/// Why a `custom_id` did not parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Longer than [`CUSTOM_ID_LIMIT`]; Discord would never have sent it.
    #[error("custom id has {0} chars; the limit is {CUSTOM_ID_LIMIT}")]
    TooLong(usize),
    /// Not the colon-separated parts its kind has.
    #[error("custom id is not `<kind>:<part>:…` with the parts its kind has")]
    Shape,
    /// First part is not a kind of button.
    #[error("unknown button kind {0:?}")]
    UnknownKind(String),
    /// A part that should be a UUID is not one.
    #[error("not a uuid: {0:?}")]
    BadUuid(String),
    /// A `rate` whose third part is not 1, 2 or 3.
    #[error("score must be 1, 2 or 3; got {0:?}")]
    BadScore(String),
    /// A legacy `pick` whose third part is not a `u8`.
    #[error("choice must be a small integer; got {0:?}")]
    BadChoice(String),
    /// A `card` whose asker is not a Discord user id.
    #[error("not a Discord user id: {0:?}")]
    BadUser(String),
    /// A `card` whose audience is neither `c` nor `p`.
    #[error("audience must be c or p; got {0:?}")]
    BadAudience(String),
    /// A `card` whose span is neither `<start>-<end>` (start below end) nor `_`.
    #[error("span must be <start>-<end> or _; got {0:?}")]
    BadSpan(String),
    /// A `card` whose digest is not 16 lowercase hex digits.
    #[error("digest must be 16 lowercase hex digits; got {0:?}")]
    BadDigest(String),
    /// A `card` or `retry` whose last part names no locale.
    #[error("not a locale: {0:?}")]
    BadLocale(String),
    /// A rating form whose last part names no feedback category.
    #[error("not a feedback category: {0:?}")]
    BadCategory(String),
}

impl ButtonAction {
    /// The `custom_id` to put on the button. Always at most [`CUSTOM_ID_LIMIT`] chars.
    #[must_use]
    pub fn to_custom_id(&self) -> String {
        self.to_string()
    }

    /// Parse a `custom_id` back. See [`ParseError`].
    ///
    /// # Errors
    /// Any malformed input; nothing is inferred from a partial match.
    pub fn parse(custom_id: &str) -> Result<Self, ParseError> {
        custom_id.parse()
    }
}

const fn audience_code(a: Audience) -> &'static str {
    match a {
        Audience::Channel => "c",
        Audience::Private => "p",
    }
}

impl fmt::Display for ButtonAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ButtonAction::Rate { call, score } => write!(f, "{RATE}:{call}:{}", *score as u8),
            ButtonAction::Pick(p) => {
                write!(f, "{CARD}:{}:{}:", p.asker, audience_code(p.audience))?;
                match p.span {
                    Some(s) => write!(f, "{}-{}", s.start, s.end)?,
                    None => f.write_str(NOT_FOUND)?,
                }
                write!(f, ":{}:{}", p.digest, p.card.into_inner().simple())?;
                write_locale(f, p.locale)
            }
            ButtonAction::Retry(r) => {
                write!(
                    f,
                    "{RETRY}:{}:{}:{}",
                    r.asker,
                    audience_code(r.audience),
                    r.digest
                )?;
                write_locale(f, r.locale)
            }
            // Never sent any more; written in the old shape for completeness.
            ButtonAction::LegacyPick => write!(f, "{LEGACY_PICK}:{}:0", Uuid::nil()),
            ButtonAction::Replace { call, score } => {
                write!(f, "{REPLACE}:{call}:{}", *score as u8)
            }
            ButtonAction::Keep { call } => write!(f, "{KEEP}:{call}"),
            ButtonAction::Category { call, fault } => {
                write!(f, "{CATEGORY}:{call}:{}", fault_digit(*fault))
            }
        }
    }
}

/// A button's `:<locale>` suffix, left out for English: an English button
/// is then the shape a build from before the suffix also reads, so a
/// rollback within the major (or the other container during a deploy)
/// still answers it.
fn write_locale(f: &mut fmt::Formatter<'_>, locale: Locale) -> fmt::Result {
    if locale == Locale::En {
        Ok(())
    } else {
        write!(f, ":{}", locale.id())
    }
}

/// `s` as a number, digits only (no sign, no blank).
fn digits<T: FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn uuid(s: &str) -> Result<Uuid, ParseError> {
    Uuid::parse_str(s).map_err(|_| ParseError::BadUuid(s.to_owned()))
}

fn parse_audience(s: &str) -> Result<Audience, ParseError> {
    match s {
        "c" => Ok(Audience::Channel),
        "p" => Ok(Audience::Private),
        other => Err(ParseError::BadAudience(other.to_owned())),
    }
}

/// A button's last part, a locale id; none is English (a button sent
/// before there was a choice).
fn parse_locale(part: Option<&str>) -> Result<Locale, ParseError> {
    match part {
        None => Ok(Locale::En),
        Some(id) => Locale::ALL
            .into_iter()
            .find(|l| l.id() == id)
            .ok_or_else(|| ParseError::BadLocale(id.to_owned())),
    }
}

fn parse_retry(rest: &str) -> Result<Retry, ParseError> {
    let mut parts = rest.split(':');
    let (Some(asker), Some(audience), Some(digest), locale, None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(ParseError::Shape);
    };
    Ok(Retry {
        asker: digits::<NonZeroU64>(asker).ok_or_else(|| ParseError::BadUser(asker.to_owned()))?,
        audience: parse_audience(audience)?,
        digest: Digest::parse(digest).ok_or_else(|| ParseError::BadDigest(digest.to_owned()))?,
        locale: parse_locale(locale)?,
    })
}

fn parse_pick(rest: &str) -> Result<Pick, ParseError> {
    let mut parts = rest.split(':');
    let (Some(asker), Some(audience), Some(span), Some(digest), Some(card), locale, None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(ParseError::Shape);
    };
    let asker = digits::<NonZeroU64>(asker).ok_or_else(|| ParseError::BadUser(asker.to_owned()))?;
    let audience = parse_audience(audience)?;
    let span = match span {
        NOT_FOUND => None,
        other => {
            let bad = || ParseError::BadSpan(other.to_owned());
            let (start, end) = other.split_once('-').ok_or_else(bad)?;
            let start = digits::<usize>(start).ok_or_else(bad)?;
            let end = digits::<usize>(end).ok_or_else(bad)?;
            Some(ByteSpan::new(start..end).ok_or_else(bad)?)
        }
    };
    let digest = Digest::parse(digest).ok_or_else(|| ParseError::BadDigest(digest.to_owned()))?;
    Ok(Pick {
        asker,
        audience,
        span,
        digest,
        card: CardId::new(uuid(card)?),
        locale: parse_locale(locale)?,
    })
}

impl FromStr for ButtonAction {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let len = s.chars().count();
        if len > CUSTOM_ID_LIMIT {
            return Err(ParseError::TooLong(len));
        }
        let (kind, rest) = s.split_once(':').ok_or(ParseError::Shape)?;
        match kind {
            RATE => {
                let (call, score) = call_and(rest, parse_score)?;
                Ok(ButtonAction::Rate { call, score })
            }
            REPLACE => {
                let (call, score) = call_and(rest, parse_score)?;
                Ok(ButtonAction::Replace { call, score })
            }
            KEEP => Ok(ButtonAction::Keep {
                call: CallId::new(uuid(rest)?),
            }),
            CATEGORY => {
                let (call, fault) = call_and(rest, parse_fault)?;
                Ok(ButtonAction::Category { call, fault })
            }
            CARD => parse_pick(rest).map(ButtonAction::Pick),
            RETRY => parse_retry(rest).map(ButtonAction::Retry),
            LEGACY_PICK => {
                let (id, n) = rest.split_once(':').ok_or(ParseError::Shape)?;
                uuid(id)?;
                n.parse::<u8>()
                    .map_err(|_| ParseError::BadChoice(n.to_owned()))?;
                Ok(ButtonAction::LegacyPick)
            }
            other => Err(ParseError::UnknownKind(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(asker: u64, audience: Audience, span: Option<Range<usize>>) -> Option<Pick> {
        Some(Pick {
            asker: NonZeroU64::new(asker)?,
            audience,
            span: match span {
                Some(r) => Some(ByteSpan::new(r)?),
                None => None,
            },
            digest: Digest::parse("0123456789abcdef")?,
            card: CardId::new(Uuid::from_u128(u128::MAX)),
            locale: Locale::En,
        })
    }

    #[test]
    fn round_trips_and_fits_the_limit() {
        let call = CallId::new(Uuid::from_u128(0xdead_beef));
        for score in [Score::Incorrect, Score::Partial, Score::Correct] {
            let a = ButtonAction::Rate { call, score };
            let id = a.to_custom_id();
            assert!(id.chars().count() <= CUSTOM_ID_LIMIT, "{id}");
            assert!(id.starts_with("rate:"));
            assert_eq!(ButtonAction::parse(&id), Ok(a));
        }
        let max = usize::from(u16::MAX);
        for audience in [Audience::Channel, Audience::Private] {
            for span in [None, Some(0..1), Some(max - 1..max)] {
                for asker in [1, u64::MAX] {
                    let p = pick(asker, audience, span.clone());
                    assert!(p.is_some(), "{asker} {span:?}");
                    let Some(p) = p else { continue };
                    let a = ButtonAction::Pick(p);
                    let id = a.to_custom_id();
                    assert!(id.chars().count() <= CUSTOM_ID_LIMIT, "{id}");
                    assert_eq!(ButtonAction::parse(&id), Ok(a));
                }
            }
        }
        // The longest there can be: every number at its maximum.
        let longest = pick(u64::MAX, Audience::Private, Some(max - 1..max))
            .map(|p| ButtonAction::Pick(p).to_custom_id());
        assert_eq!(longest.as_ref().map(String::len), Some(89), "{longest:?}");
    }

    #[test]
    fn wire_shape_is_stable() {
        let a = ButtonAction::Rate {
            call: CallId::new(Uuid::nil()),
            score: Score::Correct,
        };
        assert_eq!(
            a.to_custom_id(),
            "rate:00000000-0000-0000-0000-000000000000:3"
        );
        let p = pick(42, Audience::Channel, Some(5..9)).map(ButtonAction::Pick);
        assert_eq!(
            p.map(|p| p.to_custom_id()).as_deref(),
            Some("card:42:c:5-9:0123456789abcdef:ffffffffffffffffffffffffffffffff")
        );
        // English is written without its suffix and read with or without it.
        assert_eq!(
            ButtonAction::parse(
                "card:42:c:5-9:0123456789abcdef:ffffffffffffffffffffffffffffffff:en"
            )
            .ok(),
            p
        );
        assert!(matches!(
            ButtonAction::parse("retry:42:c:0123456789abcdef"),
            Ok(ButtonAction::Retry(Retry {
                locale: Locale::En,
                ..
            }))
        ));
        let p = pick(42, Audience::Private, None).map(ButtonAction::Pick);
        assert_eq!(
            p.map(|p| p.to_custom_id()).as_deref(),
            Some("card:42:p:_:0123456789abcdef:ffffffffffffffffffffffffffffffff")
        );
    }

    #[test]
    fn an_old_pick_button_parses_as_legacy() {
        let nil = "00000000-0000-0000-0000-000000000000";
        for id in [format!("pick:{nil}:0"), format!("pick:{nil}:4")] {
            assert_eq!(ButtonAction::parse(&id), Ok(ButtonAction::LegacyPick));
        }
        assert_eq!(
            ButtonAction::parse(&ButtonAction::LegacyPick.to_custom_id()),
            Ok(ButtonAction::LegacyPick)
        );
    }

    #[test]
    fn rejects_malformed_picks() {
        let card = "0123456789abcdef:ffffffffffffffffffffffffffffffff";
        for (id, want) in [
            ("card:42:c:5-9".to_owned(), ParseError::Shape),
            (
                format!("card:42:c:5-9:{card}:x"),
                ParseError::BadLocale("x".into()),
            ),
            (format!("card:42:c:5-9:{card}:en:x"), ParseError::Shape),
            (
                format!("card:0:c:5-9:{card}"),
                ParseError::BadUser("0".into()),
            ),
            (
                format!("card:+42:c:5-9:{card}"),
                ParseError::BadUser("+42".into()),
            ),
            (
                format!("card:18446744073709551616:c:5-9:{card}"),
                ParseError::BadUser("18446744073709551616".into()),
            ),
            (
                format!("card:42:x:5-9:{card}"),
                ParseError::BadAudience("x".into()),
            ),
            (
                format!("card:42:c:9-5:{card}"),
                ParseError::BadSpan("9-5".into()),
            ),
            (
                format!("card:42:c:5-5:{card}"),
                ParseError::BadSpan("5-5".into()),
            ),
            (
                format!("card:42:c:5-65536:{card}"),
                ParseError::BadSpan("5-65536".into()),
            ),
            (
                format!("card:42:c:-9:{card}"),
                ParseError::BadSpan("-9".into()),
            ),
            (
                format!("card:42:c:5:{card}"),
                ParseError::BadSpan("5".into()),
            ),
            (
                "card:42:c:5-9:0123456789abcdef:not-a-uuid".to_owned(),
                ParseError::BadUuid("not-a-uuid".into()),
            ),
            (
                "card:42:c:5-9:ffffffffffffffffffffffffffffffff".to_owned(),
                ParseError::Shape,
            ),
            (
                "card:42:c:5-9:0123456789ABCDEF:ffffffffffffffffffffffffffffffff".to_owned(),
                ParseError::BadDigest("0123456789ABCDEF".into()),
            ),
            (
                "card:42:c:5-9:0123456789abcde:ffffffffffffffffffffffffffffffff".to_owned(),
                ParseError::BadDigest("0123456789abcde".into()),
            ),
        ] {
            assert_eq!(ButtonAction::parse(&id), Err(want), "{id}");
        }
    }

    #[test]
    fn rejects_malformed_input() {
        let nil = "00000000-0000-0000-0000-000000000000";
        assert_eq!(ButtonAction::parse(""), Err(ParseError::Shape));
        assert_eq!(ButtonAction::parse("rate"), Err(ParseError::Shape));
        assert_eq!(
            ButtonAction::parse(&format!("rate:{nil}")),
            Err(ParseError::Shape)
        );
        assert_eq!(
            ButtonAction::parse(&format!("nuke:{nil}:1")),
            Err(ParseError::UnknownKind("nuke".into()))
        );
        assert_eq!(
            ButtonAction::parse("rate:not-a-uuid:1"),
            Err(ParseError::BadUuid("not-a-uuid".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("rate:{nil}:0")),
            Err(ParseError::BadScore("0".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("rate:{nil}:4")),
            Err(ParseError::BadScore("4".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("rate:{nil}:1:extra")),
            Err(ParseError::BadScore("1:extra".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("rate:{nil}:")),
            Err(ParseError::BadScore(String::new()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("pick:{nil}:-1")),
            Err(ParseError::BadChoice("-1".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("pick:{nil}:256")),
            Err(ParseError::BadChoice("256".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("pick:{nil}:x")),
            Err(ParseError::BadChoice("x".into()))
        );
        assert_eq!(
            ButtonAction::parse("pick:not-a-uuid:1"),
            Err(ParseError::BadUuid("not-a-uuid".into()))
        );
        assert_eq!(
            ButtonAction::parse(&format!("RATE:{nil}:1")),
            Err(ParseError::UnknownKind("RATE".into()))
        );
        let long = format!("rate:{nil}:{}", "1".repeat(200));
        assert!(
            matches!(ButtonAction::parse(&long), Err(ParseError::TooLong(n)) if n > CUSTOM_ID_LIMIT)
        );
        assert!(ParseError::Shape.to_string().contains("<kind>"));
    }

    #[test]
    fn the_rating_steps_round_trip_and_fit() -> Result<(), ParseError> {
        let call = CallId::new(Uuid::from_u128(u128::MAX));
        let mut ids = vec![ButtonAction::Keep { call }];
        for score in [Score::Incorrect, Score::Partial, Score::Correct] {
            ids.push(ButtonAction::Rate { call, score });
            ids.push(ButtonAction::Replace { call, score });
        }
        for fault in [Fault::Partial, Fault::Incorrect] {
            ids.push(ButtonAction::Category { call, fault });
        }
        for a in ids {
            let id = a.to_custom_id();
            assert!(id.chars().count() <= CUSTOM_ID_LIMIT, "{id}");
            assert_eq!(ButtonAction::parse(&id)?, a);
        }
        let mut longest = 0;
        for fault in [Fault::Partial, Fault::Incorrect] {
            for category in FeedbackCategory::ALL.map(Some).into_iter().chain([None]) {
                let form = FormId {
                    call,
                    fault,
                    category,
                };
                let id = form.to_custom_id();
                longest = longest.max(id.chars().count());
                assert_eq!(FormId::parse(&id)?, form);
            }
        }
        assert_eq!(longest, 65);
        Ok(())
    }

    /// The shapes on the wire, an old rating button's among them: a message
    /// posted before ratings had reasons gets the new flow from its old
    /// button.
    #[test]
    fn the_rating_shapes_are_stable() -> Result<(), ParseError> {
        let nil = "00000000-0000-0000-0000-000000000000";
        let call = CallId::new(Uuid::nil());
        assert_eq!(
            ButtonAction::parse(&format!("rate:{nil}:1"))?,
            ButtonAction::Rate {
                call,
                score: Score::Incorrect
            }
        );
        assert_eq!(
            ButtonAction::Replace {
                call,
                score: Score::Partial
            }
            .to_custom_id(),
            format!("rate-replace:{nil}:2")
        );
        assert_eq!(
            ButtonAction::Keep { call }.to_custom_id(),
            format!("rate-keep:{nil}")
        );
        assert_eq!(
            ButtonAction::Category {
                call,
                fault: Fault::Incorrect
            }
            .to_custom_id(),
            format!("rate-cat:{nil}:1")
        );
        assert_eq!(
            FormId {
                call,
                fault: Fault::Partial,
                category: Some(FeedbackCategory::WrongCard)
            }
            .to_custom_id(),
            format!("rate-form:{nil}:2:wrong_card")
        );
        for (bad, want) in [
            (
                format!("rate-cat:{nil}:3"),
                ParseError::BadScore("3".into()),
            ),
            (
                format!("rate-keep:{nil}:1"),
                ParseError::BadUuid(format!("{nil}:1")),
            ),
            (format!("rate-replace:{nil}"), ParseError::Shape),
        ] {
            assert_eq!(ButtonAction::parse(&bad), Err(want), "{bad}");
        }
        for (bad, want) in [
            (
                format!("rate-form:{nil}:3"),
                ParseError::BadScore("3".into()),
            ),
            (
                format!("rate-form:{nil}:1:typo"),
                ParseError::BadCategory("typo".into()),
            ),
            (format!("rate-form:{nil}:1:other:x"), ParseError::Shape),
            (
                format!("rate:{nil}:1"),
                ParseError::UnknownKind("rate".into()),
            ),
        ] {
            assert_eq!(FormId::parse(&bad), Err(want), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn a_retry_round_trips_and_its_longest_id_fits() -> Result<(), ParseError> {
        let r = ButtonAction::Retry(Retry {
            asker: NonZeroU64::MAX,
            audience: Audience::Private,
            digest: Digest::of("x"),
            locale: Locale::En,
        });
        let id = r.to_custom_id();
        assert!(id.starts_with("retry:18446744073709551615:p:"), "{id}");
        assert!(id.chars().count() <= CUSTOM_ID_LIMIT);
        assert_eq!(ButtonAction::parse(&id)?, r);
        for bad in [
            "retry:0:p:0123456789abcdef",
            "retry:1:x:0123456789abcdef",
            "retry:1:c",
        ] {
            assert!(ButtonAction::parse(bad).is_err(), "{bad}");
        }
        Ok(())
    }
}
