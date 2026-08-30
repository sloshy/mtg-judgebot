//! Typed button `custom_id`s. Discord hands a button press back as an opaque
//! string; [`ButtonAction`] is the only way one is built or read, so a click
//! either parses into an exhaustive enum or is rejected, never guessed at.
//!
//! Wire shape: `rate:<call uuid>:<1|2|3>` and `pick:<pending uuid>:<choice>`,
//! well under Discord's 100-character limit.

use std::{fmt, str::FromStr};

use judge_core::{CallId, Score};
use uuid::Uuid;

use super::pending::PendingId;

/// Discord's limit on a component `custom_id`, in characters.
pub const CUSTOM_ID_LIMIT: usize = 100;

const RATE: &str = "rate";
const PICK: &str = "pick";

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
    /// Pick candidate `choice` for the first ambiguous span of pending question `token`.
    PickCard {
        /// The pending question.
        token: PendingId,
        /// Index into the offered choices.
        choice: u8,
    },
}

/// Why a `custom_id` did not parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Longer than [`CUSTOM_ID_LIMIT`]; Discord would never have sent it.
    #[error("custom id has {0} chars; the limit is {CUSTOM_ID_LIMIT}")]
    TooLong(usize),
    /// Not three colon-separated parts.
    #[error("custom id is not `<kind>:<uuid>:<n>`")]
    Shape,
    /// First part is neither `rate` nor `pick`.
    #[error("unknown button kind {0:?}")]
    UnknownKind(String),
    /// Second part is not a UUID.
    #[error("not a uuid: {0:?}")]
    BadUuid(String),
    /// A `rate` whose third part is not 1, 2 or 3.
    #[error("score must be 1, 2 or 3; got {0:?}")]
    BadScore(String),
    /// A `pick` whose third part is not a `u8`.
    #[error("choice must be a small integer; got {0:?}")]
    BadChoice(String),
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

impl fmt::Display for ButtonAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ButtonAction::Rate { call, score } => write!(f, "{RATE}:{call}:{}", *score as u8),
            ButtonAction::PickCard { token, choice } => write!(f, "{PICK}:{token}:{choice}"),
        }
    }
}

impl FromStr for ButtonAction {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let len = s.chars().count();
        if len > CUSTOM_ID_LIMIT {
            return Err(ParseError::TooLong(len));
        }
        let mut parts = s.splitn(3, ':');
        let (Some(kind), Some(id), Some(n)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(ParseError::Shape);
        };
        let uuid = Uuid::parse_str(id).map_err(|_| ParseError::BadUuid(id.to_owned()))?;
        match kind {
            RATE => {
                let score = match n {
                    "1" => Score::Incorrect,
                    "2" => Score::Partial,
                    "3" => Score::Correct,
                    other => return Err(ParseError::BadScore(other.to_owned())),
                };
                Ok(ButtonAction::Rate {
                    call: CallId::new(uuid),
                    score,
                })
            }
            PICK => {
                let choice = n
                    .parse::<u8>()
                    .map_err(|_| ParseError::BadChoice(n.to_owned()))?;
                Ok(ButtonAction::PickCard {
                    token: PendingId::from(uuid),
                    choice,
                })
            }
            other => Err(ParseError::UnknownKind(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let token = PendingId::random();
        for choice in [0u8, 4, u8::MAX] {
            let a = ButtonAction::PickCard { token, choice };
            let id = a.to_custom_id();
            assert!(id.chars().count() <= CUSTOM_ID_LIMIT, "{id}");
            assert!(id.starts_with("pick:"));
            assert_eq!(ButtonAction::parse(&id), Ok(a));
        }
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
        let p = ButtonAction::PickCard {
            token: PendingId::from(Uuid::nil()),
            choice: 2,
        };
        assert_eq!(
            p.to_custom_id(),
            "pick:00000000-0000-0000-0000-000000000000:2"
        );
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
