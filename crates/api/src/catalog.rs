//! The HTTP API's text, one [`Messages`] per [`Locale`]: the `message` of
//! each reply that has one, and the citations' display `label`s. Clients
//! branch on the reply's `kind`, which is never translated.
//!
//! The pipeline's replies are the Discord catalog's
//! (`judge_bot::discord::catalog`), so both interfaces say the same thing;
//! an API catalog names them rather than copying them.

use std::fmt;

use judge_bot::discord::catalog as discord;
use judge_core::Locale;

/// One locale's API text.
#[derive(Clone, Copy)]
pub struct Messages {
    /// Every judge slot is taken.
    pub busy: &'static str,
    /// The caller's address used up its request window.
    pub rate_limited: &'static str,
    /// The instance is migrating for a major upgrade; nothing was asked.
    pub maintenance: &'static str,
    /// A request with a blank question.
    pub question_empty: &'static str,
    /// A question over the limit, given it.
    pub question_too_long: fn(usize) -> String,
    /// More pins than allowed, given the most.
    pub too_many_pins: fn(usize) -> String,
    /// A pin with a blank span or name.
    pub pin_empty: &'static str,
    /// A pin span or name over the limit, given it.
    pub pin_too_long: fn(usize) -> String,
    /// A ruling citation's label: its date (with a leading space) and card
    /// name, either possibly absent.
    pub ruling_label: fn(&str, Option<&str>) -> String,
    /// An Oracle-text citation's label: the face (from 2, when not the
    /// first) and card name.
    pub oracle_label: fn(Option<usize>, Option<&str>) -> String,
    /// A prior call citation's label.
    pub prior_call_label: &'static str,
    /// An `/api/` path this listener does not serve.
    pub not_routed: &'static str,
    /// A feed `cursor` that is not one the feed wrote.
    pub bad_cursor: &'static str,
    /// A feed `interface` that is none, given the ones there are.
    pub bad_interface: fn(&str) -> String,
    /// A feed `user` that is not a Discord username.
    pub bad_user: &'static str,
    /// A feed `q` over the limit, given it.
    pub bad_search: fn(usize) -> String,
    /// No public call has that id (or it is older than the feed shows).
    pub call_not_found: &'static str,
    /// The feed could not be read.
    pub feed_failed: &'static str,
    /// An admin request without the admin token, or with another.
    pub admin_unauthorized: &'static str,
    /// An admin `private` filter that is not `true` or `false`.
    pub bad_private: &'static str,
    /// An admin filter over its limit, given its name and the limit.
    pub bad_filter: fn(&str, usize) -> String,
    /// No stored call has that id.
    pub admin_call_not_found: &'static str,
    /// An admin read failed.
    pub admin_failed: &'static str,
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("api::catalog::Messages")
    }
}

/// The API text in `locale`.
#[must_use]
pub const fn messages(locale: Locale) -> &'static Messages {
    match locale {
        Locale::En => &EN,
    }
}

/// English.
pub const EN: Messages = Messages {
    busy: discord::EN.busy,
    rate_limited: "You've asked several questions in a short time. Please wait a few minutes and ask again.",
    maintenance: "Judgebot is being upgraded and will be back in a minute or two. \
Your question was not answered; please try again shortly.",
    question_empty: "question must not be empty",
    question_too_long: |n| format!("question must be at most {n} characters"),
    too_many_pins: |n| format!("at most {n} pins are accepted"),
    pin_empty: "pins must have a non-empty span and name",
    pin_too_long: |n| format!("pin spans and names must be at most {n} characters"),
    ruling_label: discord::EN.ruling_label,
    oracle_label: discord::EN.oracle_label,
    prior_call_label: "Prior call",
    not_routed: "Nothing is served at this path.",
    bad_cursor: "cursor must be the `next` of an earlier page",
    bad_interface: |all| format!("interface must be one of: {all}"),
    bad_user: "user must be a Discord username",
    bad_search: |n| format!("q must be at most {n} characters"),
    call_not_found: "There is no public call with that id.",
    feed_failed: "Sorry, the feed couldn't be read just now. Please try again.",
    admin_unauthorized: "This needs the admin token: open the address `judge-cli admin-url` prints.",
    bad_private: "private must be true or false",
    bad_filter: |name, n| format!("{name} must be at most {n} characters"),
    admin_call_not_found: "There is no call with that id.",
    admin_failed: "The database could not be read just now. Please try again.",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_locale_renders_every_message() {
        for l in Locale::ALL {
            let m = messages(l);
            for t in [
                m.busy.to_owned(),
                m.rate_limited.to_owned(),
                m.maintenance.to_owned(),
                m.question_empty.to_owned(),
                (m.question_too_long)(10),
                (m.too_many_pins)(8),
                m.pin_empty.to_owned(),
                (m.pin_too_long)(200),
                (m.ruling_label)("", Some("Blood Moon")),
                (m.oracle_label)(None, None),
                m.prior_call_label.to_owned(),
                m.not_routed.to_owned(),
                m.bad_cursor.to_owned(),
                (m.bad_interface)("web"),
                m.bad_user.to_owned(),
                (m.bad_search)(100),
                m.call_not_found.to_owned(),
                m.feed_failed.to_owned(),
                m.admin_unauthorized.to_owned(),
                m.bad_private.to_owned(),
                (m.bad_filter)("asker", 100),
                m.admin_call_not_found.to_owned(),
                m.admin_failed.to_owned(),
            ] {
                assert!(!t.trim().is_empty(), "{l}");
            }
        }
    }

    /// The web app shows this reply as the maintenance notice: it must stay
    /// the message `judge_bot::maintenance` documents for the other surfaces.
    #[test]
    fn the_english_maintenance_reply_is_the_instance_wide_one() {
        assert_eq!(EN.maintenance, judge_bot::maintenance::MESSAGE);
    }
}
