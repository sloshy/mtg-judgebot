//! `POST /api/calls/{id}/feedback`: rating an answer from the web (#27,
//! D38), served beside `POST /api/judge` (the `--api` role).
//!
//! The web has no accounts (D17). A rater is the browser tab's session id
//! (`web:<uuid>` in `ratings.user_id`), so rating again from the same tab
//! replaces the earlier rating, and only after the rater confirms it. The
//! server decides that, not the page: a request without `replace: true` is
//! answered `confirm` (409, nothing written) whenever the session already
//! has another rating on the call, and Correct over Correct is
//! `already_rated` (nothing written), whatever the client did before.
//!
//! What a web rating stores besides the rating and its reason: the name the
//! rater gave, if any, and the address ([`client_ip`], under the same
//! `API_CLIENT_IP` rules as every limiter: never `X-Forwarded-For`) and user
//! agent it came from, cleared after `FEEDBACK_IP_DAYS`. Web ratings never
//! count toward the examples (D10) and go to the operator's feedback queue
//! like Discord's.
//!
//! A call may be rated when it is public and within the feed's window
//! (`JUDGE_FEED_DAYS`); a private call, one past the window and a missing one
//! are one 404, as `GET /api/calls/{id}` answers them. `JUDGE_FEED=false`
//! does not stop rating: the answer a visitor just got is theirs to rate,
//! and its id reaches only them.

use std::{net::IpAddr, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{FromRequest, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse as _, Response},
};
use judge_bot::{feed, ingest::prune::Days, maintenance};
use judge_core::{
    CallId, Earlier, Fault, FeedbackCategory, FeedbackText, JudgeError, Next, Rated, RatedNew,
    Rater, RaterId, Rating, Reason, Score, UserAgent, WebName,
    feedback::{BadText, FEEDBACK_TEXT_MAX_CHARS, WEB_NAME_MAX_CHARS, next_step},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    catalog::{self, Messages},
    http::{App, PeerAddr, client_ip},
    shape::{self, ApiReply},
};

/// Ratings allowed per address per [`WINDOW`]: a person rates a handful of
/// answers, and each rating is a write the operator may have to read.
pub const LIMIT: u32 = 30;
/// The rating limit's window.
pub const WINDOW: Duration = Duration::from_mins(10);
/// The largest body the route reads, in bytes: two texts of 1000 characters
/// (up to 4 bytes each), a name and the rest, with room to spare.
pub const BODY_LIMIT: usize = 16 * 1024;

/// Whether the public may rate a call: behind a trait so the route is
/// tested without a database.
#[async_trait::async_trait]
pub trait Rateable: Send + Sync {
    /// Whether call `id` is public and within the window.
    async fn rateable(&self, id: CallId) -> Result<bool, JudgeError>;
}

/// [`Rateable`] over the database, within the feed's window.
#[derive(Clone, Debug)]
pub struct PgRateable {
    pool: PgPool,
    window: Option<Days>,
}

impl PgRateable {
    /// Calls younger than `window` (every call when `None`) over `pool`.
    #[must_use]
    pub const fn new(pool: PgPool, window: Option<Days>) -> Self {
        Self { pool, window }
    }
}

#[async_trait::async_trait]
impl Rateable for PgRateable {
    async fn rateable(&self, id: CallId) -> Result<bool, JudgeError> {
        feed::rateable(&self.pool, self.window, id).await
    }
}

/// A score as the API names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreName {
    /// Correct.
    Correct,
    /// Partially correct.
    Partial,
    /// Incorrect.
    Incorrect,
}

impl From<Score> for ScoreName {
    fn from(s: Score) -> Self {
        match s {
            Score::Correct => Self::Correct,
            Score::Partial => Self::Partial,
            Score::Incorrect => Self::Incorrect,
        }
    }
}

impl From<ScoreName> for Score {
    fn from(s: ScoreName) -> Self {
        match s {
            ScoreName::Correct => Self::Correct,
            ScoreName::Partial => Self::Partial,
            ScoreName::Incorrect => Self::Incorrect,
        }
    }
}

/// The request body. Unknown fields are refused, so a misspelt `replace`
/// cannot pass for a confirmation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackRequest {
    /// The browser tab's rater id (the page keeps one per tab).
    pub session_id: Uuid,
    /// The rating.
    pub score: ScoreName,
    /// For Partially correct and Incorrect, required: what kind of problem.
    #[serde(default)]
    pub category: Option<FeedbackCategory>,
    /// What is wrong (at most 1000 characters).
    #[serde(default)]
    pub wrong: Option<String>,
    /// The right answer (at most 1000 characters).
    #[serde(default)]
    pub right: Option<String>,
    /// A name to sign the reason with (at most 100 characters), shown to
    /// the operator only.
    #[serde(default)]
    pub name: Option<String>,
    /// The rater confirmed replacing the rating they gave earlier.
    #[serde(default)]
    pub replace: bool,
    /// The language for the reply's `message`, ahead of `Accept-Language`.
    #[serde(default)]
    pub locale: Option<String>,
}

/// An earlier rating, as a `confirm` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EarlierView {
    /// Its score.
    pub score: ScoreName,
    /// Its reason's category, when it had one.
    pub category: Option<FeedbackCategory>,
}

impl From<Earlier> for EarlierView {
    fn from(e: Earlier) -> Self {
        Self {
            score: e.score.into(),
            category: e.category,
        }
    }
}

/// What a rating request came to, tagged by `kind`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedbackReply {
    /// Stored (200).
    Rated {
        /// For the rater.
        message: String,
        /// The rating stored.
        score: ScoreName,
        /// The rating it replaced, when there was one.
        replaced: Option<ScoreName>,
    },
    /// Correct again over Correct: nothing changed (200).
    AlreadyRated {
        /// For the rater.
        message: String,
    },
    /// The session has another rating on the call: send again with
    /// `replace: true` to replace it (409, nothing written).
    Confirm {
        /// The question to put to the rater.
        message: String,
        /// The rating on record.
        earlier: EarlierView,
    },
}

/// The rating a valid request asks for, or the message for a 400.
///
/// # Errors
/// A Correct with a reason or a name, a fault without its category, or a
/// text or name over its limit or holding a control character.
pub fn rating(m: &Messages, req: &FeedbackRequest) -> Result<(Rating, Option<WebName>), String> {
    let blank = |v: &Option<String>| v.as_deref().is_none_or(|t| t.trim().is_empty());
    let name = match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        None => None,
        Some(n) => Some(
            WebName::try_new(n).map_err(|_| (m.bad_feedback_text)("name", WEB_NAME_MAX_CHARS))?,
        ),
    };
    let fault = Fault::of(req.score.into());
    let Some(fault) = fault else {
        if req.category.is_some() || !blank(&req.wrong) || !blank(&req.right) || name.is_some() {
            return Err(m.correct_takes_no_reason.to_owned());
        }
        return Ok((Rating::Correct, None));
    };
    let category = req.category.ok_or_else(|| m.category_required.to_owned())?;
    let text = |raw: &Option<String>, field: &str| {
        raw.as_deref()
            .map_or(Ok(None), FeedbackText::optional)
            .map_err(|e| match e {
                BadText::TooLong { .. } | BadText::Control => {
                    (m.bad_feedback_text)(field, FEEDBACK_TEXT_MAX_CHARS)
                }
            })
    };
    Ok((
        Rating::faulted(
            fault,
            Reason {
                category,
                wrong: text(&req.wrong, "wrong")?,
                right: text(&req.right, "right")?,
            },
        ),
        name,
    ))
}

/// The body, read as JSON with axum's own status when it cannot be (not
/// JSON, an unknown field, too large), as an [`ApiReply::Error`] whose
/// message is the catalog's `unreadable`.
pub(crate) struct FeedbackBody(FeedbackRequest);

impl FromRequest<Arc<App>> for FeedbackBody {
    type Rejection = (StatusCode, Json<ApiReply>);

    async fn from_request(req: Request, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        let locale = crate::http::header_locale(req.headers(), app.locale());
        match Json::<FeedbackRequest>::from_request(req, app).await {
            Ok(Json(body)) => Ok(Self(body)),
            Err(e) => {
                // A body that is JSON but not this one (an unknown field, a
                // wrong type) is the caller's mistake like any other: 400,
                // where axum says 422.
                let status = match e {
                    JsonRejection::JsonDataError(_) => StatusCode::BAD_REQUEST,
                    _ => e.status(),
                };
                Err((
                    status,
                    Json(ApiReply::Error {
                        message: (catalog::messages(locale).unreadable)(&e.body_text()),
                    }),
                ))
            }
        }
    }
}

fn reply(status: StatusCode, body: impl Serialize) -> Response {
    (status, Json(body)).into_response()
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    reply(
        status,
        ApiReply::Error {
            message: message.into(),
        },
    )
}

/// `POST /api/calls/{id}/feedback`. In order: the body, the maintenance
/// reply, the address's window, the call, the earlier rating and what it
/// says ([`next_step`]), then the write.
pub(crate) async fn feedback_route(
    State(app): State<Arc<App>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Path(id): Path<String>,
    FeedbackBody(req): FeedbackBody,
) -> Response {
    let locale = shape::request_locale(
        req.locale.as_deref(),
        headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok()),
        app.locale(),
    );
    let m = catalog::messages(locale);
    let (rating, name) = match rating(m, &req) {
        Ok(ok) => ok,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    if !app.is_open() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::RETRY_AFTER,
                maintenance::RETRY_AFTER.as_secs().to_string(),
            )],
            Json(ApiReply::Maintenance {
                message: m.maintenance.to_owned(),
                retry_after_secs: maintenance::RETRY_AFTER.as_secs(),
            }),
        )
            .into_response();
    }
    let ip: IpAddr = client_ip(&headers, peer, app.client_ip_source());
    if !app.allow_rating(ip) {
        tracing::info!(%ip, "ratings rate limited");
        return reply(
            StatusCode::TOO_MANY_REQUESTS,
            ApiReply::RateLimited {
                message: m.feedback_rate_limited.to_owned(),
            },
        );
    }
    let (Some(rateable), Ok(call)) = (app.rateable(), Uuid::try_parse(&id).map(CallId::new)) else {
        return error(StatusCode::NOT_FOUND, m.call_not_found);
    };
    match rateable.rateable(call).await {
        Ok(true) => {}
        Ok(false) => return error(StatusCode::NOT_FOUND, m.call_not_found),
        Err(e) => return failed(m, &e),
    }
    let rater_id = RaterId::Web(req.session_id);
    let earlier = match app.store().earlier_rating(call, rater_id).await {
        Ok(e) => e,
        Err(e) => return failed(m, &e),
    };
    // A first look: the common cases answer without a write.
    if let Some(answer) = decline(m, &rating, earlier, req.replace) {
        return answer;
    }
    let rater = Rater::Web {
        session: req.session_id,
        name,
        ip: Some(ip),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .and_then(UserAgent::keep),
    };
    // Unconfirmed, the write itself refuses to replace: a second request from
    // the same session that raced this one past the read above is answered
    // as if it had seen the rating, never written over it.
    let written = if req.replace {
        app.store()
            .rate(call, &rater, &rating)
            .await
            .map(|r| match r {
                Rated::Recorded => RatedNew::Recorded,
                Rated::CallGone => RatedNew::CallGone,
            })
    } else {
        app.store().rate_if_new(call, &rater, &rating).await
    };
    match written {
        Ok(RatedNew::Recorded) => {
            let category = rating.reason().map(|r| r.category);
            tracing::info!(
                %call,
                %ip,
                score = rating.score() as u8,
                category = category.map(FeedbackCategory::id),
                "rated from the web"
            );
            let template = if app.public_feedback() && category.is_some() {
                m.rated_public
            } else {
                m.rated
            };
            reply(
                StatusCode::OK,
                FeedbackReply::Rated {
                    message: template(
                        (m.rating_label)(rating.score()),
                        category.map(m.category_label),
                    ),
                    score: rating.score().into(),
                    replaced: earlier.map(|e| e.score.into()),
                },
            )
        }
        Ok(RatedNew::Taken(e)) => decline(m, &rating, Some(e), false).unwrap_or_else(|| {
            // `next_step` over an earlier rating always declines unconfirmed.
            error(StatusCode::CONFLICT, m.feedback_failed)
        }),
        Ok(RatedNew::CallGone) => error(StatusCode::NOT_FOUND, m.call_not_found),
        Err(e) => failed(m, &e),
    }
}

/// The answer that writes nothing, when `earlier` calls for one: Correct
/// over Correct is `already_rated`, and any other earlier rating, unless
/// `replace` confirms it, a 409 `confirm`.
fn decline(
    m: &Messages,
    rating: &Rating,
    earlier: Option<Earlier>,
    replace: bool,
) -> Option<Response> {
    match next_step(rating.score(), earlier) {
        Next::AlreadyRecorded => Some(reply(
            StatusCode::OK,
            FeedbackReply::AlreadyRated {
                message: m.already_rated.to_owned(),
            },
        )),
        Next::Confirm(e) if !replace => Some(reply(
            StatusCode::CONFLICT,
            FeedbackReply::Confirm {
                message: (m.confirm_replace)(
                    (m.rating_label)(e.score),
                    e.category.map(m.category_label),
                    (m.rating_label)(rating.score()),
                ),
                earlier: e.into(),
            },
        )),
        Next::Confirm(_) | Next::Record | Next::Form(_) => None,
    }
}

fn failed(m: &Messages, e: &JudgeError) -> Response {
    tracing::warn!(
        error = format_args!("{e:#}"),
        "POST /api/calls/{{id}}/feedback failed"
    );
    error(StatusCode::INTERNAL_SERVER_ERROR, m.feedback_failed)
}
