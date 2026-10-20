//! Ratings with reasons (#27, D38): what a rater says about an answer, and
//! the operator's work queue those reasons arrive in.
//!
//! A rating is one of three, and only the two that find fault carry a
//! [`Reason`]: [`Rating::Correct`] has no field to put one in, so a "Correct,
//! because…" cannot be built, stored or shown. The reason is for the
//! operator, never the model: it reaches the feedback queue beside the call's
//! question, answer, citations and cards, and nothing in retrieval reads it.
//!
//! A rater is a Discord user or a web browser session ([`Rater`]); only a
//! Discord rating shapes retrieval (`calls_rated`, D10).

use std::{fmt, net::IpAddr, num::NonZeroU64};

use nutype::nutype;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Score;

/// The longest "what's wrong" or "the right answer", in characters.
pub const FEEDBACK_TEXT_MAX_CHARS: usize = 1000;
/// The longest name a web rater may give, in characters.
pub const WEB_NAME_MAX_CHARS: usize = 100;
/// The longest user agent kept, in characters; a longer one is cut.
pub const USER_AGENT_MAX_CHARS: usize = 512;
/// The longest operator note on a queue item, in characters.
pub const QUEUE_NOTE_MAX_CHARS: usize = 2000;
/// The longest "fixed by" on a queue item, in characters.
pub const FIXED_BY_MAX_CHARS: usize = 200;
/// The prefix of a web rater's stored id: `web:<session uuid>`. A Discord
/// user id is all digits, so the two can never collide.
pub const WEB_RATER_PREFIX: &str = "web:";

/// What kind of problem a rater found. Exhaustive: the Discord form's
/// options, the stored `ratings.category` and the queue's filter are all
/// this list.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackCategory {
    /// The answer reached the wrong conclusion.
    WrongConclusion,
    /// It answered only part of the question.
    MissedPart,
    /// A citation does not support what the answer says.
    BadCitation,
    /// It was about the wrong card.
    WrongCard,
    /// It misread what was asked.
    MisreadQuestion,
    /// It refused a question it should have answered.
    WrongRefusal,
    /// It is out of date (a newer CR, an erratum).
    Outdated,
    /// Anything else.
    Other,
}

impl FeedbackCategory {
    /// Every category, in the order the form lists them.
    pub const ALL: [Self; 8] = [
        Self::WrongConclusion,
        Self::MissedPart,
        Self::BadCitation,
        Self::WrongCard,
        Self::MisreadQuestion,
        Self::WrongRefusal,
        Self::Outdated,
        Self::Other,
    ];

    /// The stored id (`ratings.category`), equal to the serde name, at most
    /// 16 ASCII characters (it travels in Discord custom ids).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::WrongConclusion => "wrong_conclusion",
            Self::MissedPart => "missed_part",
            Self::BadCitation => "bad_citation",
            Self::WrongCard => "wrong_card",
            Self::MisreadQuestion => "misread_question",
            Self::WrongRefusal => "wrong_refusal",
            Self::Outdated => "outdated",
            Self::Other => "other",
        }
    }

    /// The category whose id is `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.id() == id)
    }
}

impl fmt::Display for FeedbackCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Who rated: a Discord user or a web browser session. Stored as
/// `ratings.rater_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RaterKind {
    /// A Discord user, by user id.
    Discord,
    /// A browser session on the web app, by its session id.
    Web,
}

impl RaterKind {
    /// Both kinds.
    pub const ALL: [Self; 2] = [Self::Discord, Self::Web];

    /// The stored id, equal to the serde name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Web => "web",
        }
    }

    /// The kind whose id is `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }
}

impl fmt::Display for RaterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Whether `s` is text a person typed: no control characters but line
/// breaks and tabs. Postgres refuses U+0000 in `text`, and the rest have no
/// business in a note.
fn typed(s: &str) -> bool {
    !s.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

/// "What's wrong" or "the right answer": trimmed, 1 to
/// [`FEEDBACK_TEXT_MAX_CHARS`] characters, no control characters but line
/// breaks and tabs. A blank field is no text at all
/// ([`FeedbackText::optional`]), never an empty one.
#[nutype(
    sanitize(trim),
    validate(not_empty, len_char_max = 1000, predicate = typed),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct FeedbackText(String);

/// Why a typed field was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BadText {
    /// Longer than its limit.
    #[error("at most {limit} characters")]
    TooLong {
        /// The limit, in characters.
        limit: usize,
    },
    /// It holds a control character other than a line break or a tab
    /// (U+0000, which Postgres refuses, among them).
    #[error("text without control characters")]
    Control,
}

impl FeedbackText {
    /// `raw` trimmed, `None` when that leaves nothing.
    ///
    /// # Errors
    /// [`BadText`]: past [`FEEDBACK_TEXT_MAX_CHARS`], or a control character.
    pub fn optional(raw: &str) -> Result<Option<Self>, BadText> {
        optional(raw, FEEDBACK_TEXT_MAX_CHARS, |s| Self::try_new(s).ok())
    }
}

/// `raw` trimmed into `make`, `None` when blank; [`BadText::TooLong`] past
/// `limit`, else [`BadText::Control`] when `make` refuses it (its one other
/// check).
fn optional<T>(
    raw: &str,
    limit: usize,
    make: impl FnOnce(&str) -> Option<T>,
) -> Result<Option<T>, BadText> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > limit {
        return Err(BadText::TooLong { limit });
    }
    make(trimmed).map(Some).ok_or(BadText::Control)
}

/// The reason a rater gave for a rating that finds fault: a category, and
/// optionally what is wrong and what the right answer is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    /// What kind of problem.
    pub category: FeedbackCategory,
    /// What is wrong, in the rater's words.
    pub wrong: Option<FeedbackText>,
    /// The right answer, in the rater's words.
    pub right: Option<FeedbackText>,
}

/// A rating that finds fault, the two that take a [`Reason`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    /// Partially correct.
    Partial,
    /// Incorrect.
    Incorrect,
}

impl Fault {
    /// The score it stands for.
    #[must_use]
    pub const fn score(self) -> Score {
        match self {
            Self::Partial => Score::Partial,
            Self::Incorrect => Score::Incorrect,
        }
    }

    /// The fault `score` is, `None` for [`Score::Correct`].
    #[must_use]
    pub const fn of(score: Score) -> Option<Self> {
        match score {
            Score::Partial => Some(Self::Partial),
            Score::Incorrect => Some(Self::Incorrect),
            Score::Correct => None,
        }
    }
}

/// One rater's rating of a call. Only the faulting two carry a reason, so
/// "Correct with a reason" cannot be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rating {
    /// The answer was right.
    Correct,
    /// Partially right, and why.
    Partial(Reason),
    /// Wrong, and why.
    Incorrect(Reason),
}

impl Rating {
    /// `fault` with `reason`.
    #[must_use]
    pub const fn faulted(fault: Fault, reason: Reason) -> Self {
        match fault {
            Fault::Partial => Self::Partial(reason),
            Fault::Incorrect => Self::Incorrect(reason),
        }
    }

    /// The score.
    #[must_use]
    pub const fn score(&self) -> Score {
        match self {
            Self::Correct => Score::Correct,
            Self::Partial(_) => Score::Partial,
            Self::Incorrect(_) => Score::Incorrect,
        }
    }

    /// The reason, for a rating that finds fault.
    #[must_use]
    pub const fn reason(&self) -> Option<&Reason> {
        match self {
            Self::Correct => None,
            Self::Partial(r) | Self::Incorrect(r) => Some(r),
        }
    }
}

/// A name a web rater gave: trimmed, 1 to [`WEB_NAME_MAX_CHARS`] characters,
/// no control characters.
#[nutype(
    sanitize(trim),
    validate(not_empty, len_char_max = 100, predicate = |s: &str| !s.chars().any(char::is_control)),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct WebName(String);

/// A browser's user agent as kept: at most [`USER_AGENT_MAX_CHARS`]
/// characters, cut rather than refused (it is the browser's, not the
/// rater's), no control characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserAgent(String);

impl UserAgent {
    /// `raw` with control characters dropped, trimmed and cut to the limit;
    /// `None` when nothing is left.
    #[must_use]
    pub fn keep(raw: &str) -> Option<Self> {
        let clean: String = raw
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .chars()
            .take(USER_AGENT_MAX_CHARS)
            .collect();
        (!clean.is_empty()).then_some(Self(clean))
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who rates: the stored `ratings.user_id`, one row per call and rater.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RaterId {
    /// A Discord user id.
    Discord(NonZeroU64),
    /// A web browser session's id (the web app's client UUID).
    Web(Uuid),
}

impl RaterId {
    /// Which kind of rater.
    #[must_use]
    pub const fn kind(self) -> RaterKind {
        match self {
            Self::Discord(_) => RaterKind::Discord,
            Self::Web(_) => RaterKind::Web,
        }
    }

    /// The id as stored: the Discord user id, or `web:<session uuid>`.
    #[must_use]
    pub fn stored(self) -> String {
        match self {
            Self::Discord(id) => id.to_string(),
            Self::Web(session) => format!("{WEB_RATER_PREFIX}{session}"),
        }
    }
}

/// A rater with what is stored about them besides the rating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rater {
    /// A Discord user; `judge` when they held the judge role as they rated.
    Discord {
        /// Their user id.
        user: NonZeroU64,
        /// Whether the rating counts as a judge's ruling.
        judge: bool,
    },
    /// A web browser session, with what the request carried. The address
    /// and user agent are cleared after `FEEDBACK_IP_DAYS`.
    Web {
        /// The session id.
        session: Uuid,
        /// The name they gave, if any.
        name: Option<WebName>,
        /// The address the rating came from.
        ip: Option<IpAddr>,
        /// The browser's user agent.
        user_agent: Option<UserAgent>,
    },
}

impl Rater {
    /// The stored id.
    #[must_use]
    pub const fn id(&self) -> RaterId {
        match self {
            Self::Discord { user, .. } => RaterId::Discord(*user),
            Self::Web { session, .. } => RaterId::Web(*session),
        }
    }

    /// Whether the rating counts as a judge's ruling (never for the web).
    #[must_use]
    pub const fn is_judge(&self) -> bool {
        matches!(self, Self::Discord { judge: true, .. })
    }
}

/// A rater's rating of a call as stored, for "you already rated this…".
/// A rating from before reasons existed has a fault and no category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Earlier {
    /// The score.
    pub score: Score,
    /// The reason's category, when one was given.
    pub category: Option<FeedbackCategory>,
}

/// What a click on a rating button leads to ([`next_step`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// The same Correct is already recorded: say so, change nothing.
    AlreadyRecorded,
    /// The rater has another rating, or a fault they may want to restate:
    /// ask before replacing it.
    Confirm(Earlier),
    /// Record Correct.
    Record,
    /// Ask for the reason of this fault.
    Form(Fault),
}

/// What clicking `clicked` leads to, given the rater's `earlier` rating of
/// the call. A rating is replaced only after a confirmation, and Correct
/// twice is no change at all.
#[must_use]
pub fn next_step(clicked: Score, earlier: Option<Earlier>) -> Next {
    match (earlier, Fault::of(clicked)) {
        (Some(e), None) if e.score == Score::Correct => Next::AlreadyRecorded,
        (Some(e), _) => Next::Confirm(e),
        (None, None) => Next::Record,
        (None, Some(fault)) => Next::Form(fault),
    }
}

/// [`next_step`] once the rater chose Replace: the confirmation is behind
/// them.
#[must_use]
pub const fn replace_step(clicked: Score) -> Next {
    match Fault::of(clicked) {
        None => Next::Record,
        Some(fault) => Next::Form(fault),
    }
}

/// Where a queue item stands. Stored as `feedback_queue.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    /// Waiting for the operator (new, or reopened by a replaced reason).
    Open,
    /// The operator fixed it (a note, an alias, a prompt change…).
    Resolved,
    /// The operator decided there is nothing to fix.
    Dismissed,
}

impl QueueStatus {
    /// Every status.
    pub const ALL: [Self; 3] = [Self::Open, Self::Resolved, Self::Dismissed];

    /// The stored id, equal to the serde name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Dismissed => "dismissed",
        }
    }

    /// The status whose id is `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }
}

impl fmt::Display for QueueStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// How the operator closes a queue item: the two statuses that are not
/// [`QueueStatus::Open`], so an item cannot be "closed" as open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Closing {
    /// Fixed.
    Resolved,
    /// Nothing to fix.
    Dismissed,
}

impl Closing {
    /// The status it leaves the item in.
    #[must_use]
    pub const fn status(self) -> QueueStatus {
        match self {
            Self::Resolved => QueueStatus::Resolved,
            Self::Dismissed => QueueStatus::Dismissed,
        }
    }
}

/// The operator's note on a queue item: trimmed, 1 to
/// [`QUEUE_NOTE_MAX_CHARS`] characters, no control characters but line
/// breaks and tabs.
#[nutype(
    sanitize(trim),
    validate(not_empty, len_char_max = 2000, predicate = typed),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct QueueNote(String);

/// What fixed a queue item (a commit, a notes.yaml entry, an alias):
/// trimmed, 1 to [`FIXED_BY_MAX_CHARS`] characters, no control characters
/// but line breaks and tabs.
#[nutype(
    sanitize(trim),
    validate(not_empty, len_char_max = 200, predicate = typed),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct FixedBy(String);

impl FixedBy {
    /// `raw` trimmed, `None` when that leaves nothing.
    ///
    /// # Errors
    /// [`BadText`]: past [`FIXED_BY_MAX_CHARS`], or a control character.
    pub fn optional(raw: &str) -> Result<Option<Self>, BadText> {
        optional(raw, FIXED_BY_MAX_CHARS, |s| Self::try_new(s).ok())
    }
}

/// How the operator closes an item: resolved or dismissed, with a note and
/// optionally what fixed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Close {
    /// Resolved or dismissed.
    pub closing: Closing,
    /// Why.
    pub note: QueueNote,
    /// What fixed it.
    pub fixed_by: Option<FixedBy>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_ids_round_trip_and_fit_a_custom_id() {
        for c in FeedbackCategory::ALL {
            assert_eq!(FeedbackCategory::parse(c.id()), Some(c));
            assert!(c.id().len() <= 16 && c.id().is_ascii(), "{c}");
            let json = serde_json::to_value(c).ok();
            assert_eq!(json.as_ref().and_then(|v| v.as_str()), Some(c.id()));
        }
        assert_eq!(FeedbackCategory::parse("WRONG_CARD"), None);
        for s in QueueStatus::ALL {
            assert_eq!(QueueStatus::parse(s.id()), Some(s));
        }
        for k in RaterKind::ALL {
            assert_eq!(RaterKind::parse(k.id()), Some(k));
        }
    }

    #[test]
    fn a_blank_text_is_absent_and_a_long_one_refused() {
        assert_eq!(FeedbackText::optional("   \n"), Ok(None));
        let one = FeedbackText::optional("  it cited 702.19  ");
        assert_eq!(
            one.ok().flatten().map(|t| t.to_string()).as_deref(),
            Some("it cited 702.19")
        );
        let max = "é".repeat(FEEDBACK_TEXT_MAX_CHARS);
        assert!(matches!(FeedbackText::optional(&max), Ok(Some(_))));
        let over = format!("{max}x");
        assert_eq!(
            FeedbackText::optional(&over),
            Err(BadText::TooLong {
                limit: FEEDBACK_TEXT_MAX_CHARS
            })
        );
        assert!(FeedbackText::try_new("   ").is_err());
        assert_eq!(FixedBy::optional(""), Ok(None));
    }

    /// A NUL (which Postgres `text` refuses) or another control character
    /// is refused with a reason, never dropped; line breaks and tabs are
    /// what people type.
    #[test]
    fn control_characters_are_refused_and_line_breaks_kept() {
        for bad in ["a\u{0}b", "bell\u{7}", "esc\u{1b}[0m", "del\u{7f}"] {
            assert_eq!(
                FeedbackText::optional(bad),
                Err(BadText::Control),
                "{bad:?}"
            );
            assert_eq!(FixedBy::optional(bad), Err(BadText::Control), "{bad:?}");
            assert!(QueueNote::try_new(bad).is_err(), "{bad:?}");
        }
        let typed = "line one\r\nline two\twith a tab";
        assert!(matches!(FeedbackText::optional(typed), Ok(Some(_))));
        assert!(QueueNote::try_new(typed).is_ok());
    }

    #[test]
    fn only_a_fault_carries_a_reason() {
        let reason = Reason {
            category: FeedbackCategory::WrongCard,
            wrong: None,
            right: None,
        };
        assert_eq!(Rating::Correct.reason(), None);
        let r = Rating::faulted(Fault::Partial, reason.clone());
        assert_eq!(r.score(), Score::Partial);
        assert_eq!(r.reason(), Some(&reason));
        assert_eq!(Fault::of(Score::Correct), None);
        assert_eq!(
            Fault::of(Score::Incorrect).map(Fault::score),
            Some(Score::Incorrect)
        );
    }

    /// The decision table: Correct twice is no change; any other earlier
    /// rating asks first; nothing earlier records Correct or asks the reason.
    #[test]
    fn a_click_records_asks_or_confirms() {
        let earlier = |score, category| Some(Earlier { score, category });
        let correct = earlier(Score::Correct, None);
        let wrong_card = earlier(Score::Partial, Some(FeedbackCategory::WrongCard));
        let legacy = earlier(Score::Incorrect, None);
        assert_eq!(next_step(Score::Correct, correct), Next::AlreadyRecorded);
        for clicked in [Score::Partial, Score::Incorrect] {
            assert!(matches!(next_step(clicked, correct), Next::Confirm(_)));
        }
        for e in [wrong_card, legacy] {
            for clicked in [Score::Correct, Score::Partial, Score::Incorrect] {
                assert!(
                    matches!(next_step(clicked, e), Next::Confirm(x) if Some(x) == e),
                    "{clicked:?} over {e:?}"
                );
            }
        }
        assert_eq!(next_step(Score::Correct, None), Next::Record);
        assert_eq!(next_step(Score::Partial, None), Next::Form(Fault::Partial));
        assert_eq!(
            next_step(Score::Incorrect, None),
            Next::Form(Fault::Incorrect)
        );
        assert_eq!(replace_step(Score::Correct), Next::Record);
        assert_eq!(replace_step(Score::Incorrect), Next::Form(Fault::Incorrect));
    }

    #[test]
    fn rater_ids_are_stored_by_kind() {
        let discord = RaterId::Discord(NonZeroU64::MIN);
        assert_eq!(discord.stored(), "1");
        assert_eq!(discord.kind(), RaterKind::Discord);
        let web = RaterId::Web(Uuid::nil());
        assert_eq!(web.stored(), "web:00000000-0000-0000-0000-000000000000");
        assert_eq!(web.kind(), RaterKind::Web);
        let rater = Rater::Web {
            session: Uuid::nil(),
            name: None,
            ip: None,
            user_agent: None,
        };
        assert!(!rater.is_judge());
        assert_eq!(rater.id(), web);
    }

    #[test]
    fn a_user_agent_is_cut_not_refused() {
        assert_eq!(UserAgent::keep(" \u{7} "), None);
        let long = "a".repeat(USER_AGENT_MAX_CHARS + 50);
        assert_eq!(
            UserAgent::keep(&long).map(|u| u.as_str().chars().count()),
            Some(USER_AGENT_MAX_CHARS)
        );
        assert!(WebName::try_new("bob\u{0}").is_err());
        assert!(WebName::try_new(" Bob ").is_ok());
    }
}
