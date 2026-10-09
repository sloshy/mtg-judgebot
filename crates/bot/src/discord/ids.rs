//! Typed button `custom_id`s. Discord hands a button press back as an opaque
//! string; [`ButtonAction`] is the only way one is built or read, so a click
//! either parses into an exhaustive enum or is rejected, never guessed at.
//!
//! Wire shapes, all well under Discord's 100-character limit (the longest
//! possible `card` id is 89 characters, a test holds it):
//!
//! * `rate:<call uuid>:<1|2|3>`
//! * `card:<asker>:<c|p>:<start>-<end>|_:<digest>:<oracle id>`: pick that
//!   card for the ambiguous span at bytes `start..end` of the question (`_`:
//!   the span was not found in it, so the pick appends the name). The asker
//!   is a Discord user id, `c`/`p` the [`Audience`], the digest 16 hex digits
//!   of the prompt's content ([`Digest`]), the oracle id 32 hex digits. The
//!   question itself is not here: it is read back from the message the button
//!   is on ([`super::render::PickPrompt`]), whose content must match the digest.
//! * `pick:<uuid>:<n>`: the shape a pick had while picks were held in memory.
//!   It parses, to [`ButtonAction::LegacyPick`], so a button sent before the
//!   upgrade is answered as expired rather than as unrecognised.

use std::{fmt, num::NonZeroU64, ops::Range, str::FromStr};

use judge_core::{CallId, CardId, Score};
use uuid::Uuid;

use super::{Audience, pick::Digest};

/// Discord's limit on a component `custom_id`, in characters.
pub const CUSTOM_ID_LIMIT: usize = 100;

const RATE: &str = "rate";
const CARD: &str = "card";
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
    /// A pick button from a release that held the question in memory
    /// (`pick:<uuid>:<n>`). Nothing can answer it any more.
    LegacyPick,
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
                write!(f, ":{}:{}", p.digest, p.card.into_inner().simple())
            }
            // Never sent any more; written in the old shape for completeness.
            ButtonAction::LegacyPick => write!(f, "{LEGACY_PICK}:{}:0", Uuid::nil()),
        }
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

fn parse_pick(rest: &str) -> Result<Pick, ParseError> {
    let mut parts = rest.split(':');
    let (Some(asker), Some(audience), Some(span), Some(digest), Some(card), None) = (
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
    let audience = match audience {
        "c" => Audience::Channel,
        "p" => Audience::Private,
        other => return Err(ParseError::BadAudience(other.to_owned())),
    };
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
                let (id, n) = rest.split_once(':').ok_or(ParseError::Shape)?;
                let call = CallId::new(uuid(id)?);
                let score = match n {
                    "1" => Score::Incorrect,
                    "2" => Score::Partial,
                    "3" => Score::Correct,
                    other => return Err(ParseError::BadScore(other.to_owned())),
                };
                Ok(ButtonAction::Rate { call, score })
            }
            CARD => parse_pick(rest).map(ButtonAction::Pick),
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
            (format!("card:42:c:5-9:{card}:x"), ParseError::Shape),
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
}
