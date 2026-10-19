//! The languages the interfaces speak, and core's own messages.
//!
//! A [`Locale`] is chosen per request from what the asker's client says
//! (Discord's user and guild locale, a browser's language) and falls back to
//! the instance's `JUDGE_LOCALE`. Each surface keeps one catalog per locale,
//! a struct with a field per message, so a second locale is a new
//! [`Locale`] variant plus one catalog value per surface, and the compiler
//! lists every missing message. Plurals, durations and dates are functions in
//! the catalog, so each locale writes its own rules.
//!
//! Only English ships. Tracing logs stay English everywhere, so they can be
//! grepped and pasted into bug reports. The rules material, the model prompts
//! and the answers stay English too.
//!
//! This module holds core's catalog ([`Messages`]): the source offer, the
//! operator contact and the data freshness lines, which several surfaces
//! show.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `JUDGE_LOCALE`: the instance's language when the asker's is not one of
/// [`Locale::ALL`].
pub const LOCALE_ENV: &str = "JUDGE_LOCALE";

/// A language every surface has a catalog for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    /// English.
    #[default]
    En,
}

impl Locale {
    /// Every locale, in declaration order.
    pub const ALL: [Self; 1] = [Self::En];

    /// The id stored and sent (`calls.locale`, `JUDGE_LOCALE`, the web app's
    /// catalogs): the language subtag.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::En => "en",
        }
    }

    /// The locale a language tag names (`en`, `en-GB`, `EN_us`), by its
    /// language subtag; `None` for a language with no catalog.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        let language = tag
            .trim()
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        Self::ALL.into_iter().find(|l| l.id() == language)
    }

    /// The first of `preferred` (most preferred first) that is a locale,
    /// else `fallback`.
    #[must_use]
    pub fn negotiate<'a>(preferred: impl IntoIterator<Item = &'a str>, fallback: Self) -> Self {
        preferred
            .into_iter()
            .find_map(Self::from_tag)
            .unwrap_or(fallback)
    }

    /// Parse `JUDGE_LOCALE`: blank is the default, English.
    ///
    /// # Errors
    /// The value, when it names no locale.
    pub fn parse_setting(raw: Option<&str>) -> Result<Self, String> {
        match raw.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(Self::default()),
            Some(v) => Self::from_tag(v).ok_or_else(|| v.to_owned()),
        }
    }

    /// Every locale's id, comma-separated, for an error message.
    #[must_use]
    pub fn supported() -> String {
        Self::ALL.map(Self::id).join(", ")
    }

    /// Core's messages in this locale.
    #[must_use]
    pub const fn messages(self) -> &'static Messages {
        match self {
            Self::En => &EN,
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The facts of the source offer, for [`Messages::notice`].
#[derive(Clone, Copy, Debug)]
pub struct Offer<'a> {
    /// The program's name.
    pub program: &'a str,
    /// The copyright line.
    pub copyright: &'a str,
    /// The licence, in words.
    pub license_name: &'a str,
    /// Its SPDX identifier.
    pub license_spdx: &'a str,
    /// Where to read it.
    pub license_url: &'a str,
    /// Where the source is.
    pub repository: &'a str,
    /// The revision phrase ([`Messages::revision`]).
    pub revision: &'a str,
}

/// The build a revision phrase describes.
#[derive(Clone, Copy, Debug)]
pub enum Revision<'a> {
    /// A commit, short hash, and whether the tree had uncommitted changes.
    Known {
        /// The short hash.
        short: &'a str,
        /// Built with uncommitted changes.
        dirty: bool,
    },
    /// The build did not know its commit.
    Unknown,
}

/// How to reach the operator, for [`Messages::contact`].
#[derive(Clone, Copy, Debug)]
pub enum Contact<'a> {
    /// An email address and a Discord username.
    Both {
        /// The address.
        email: &'a str,
        /// The username, without `@`.
        discord: &'a str,
    },
    /// An email address.
    Email(&'a str),
    /// A Discord username, without `@`.
    Discord(&'a str),
}

/// Core's text in one language.
#[derive(Clone, Copy)]
pub struct Messages {
    /// The revision phrase: `commit a37d495`.
    pub revision: fn(Revision<'_>) -> String,
    /// The whole source offer as one paragraph.
    pub notice: fn(&Offer<'_>) -> String,
    /// Whom to contact for support.
    pub contact: fn(Contact<'_>) -> String,
    /// The CR release line, given the release date or `None` for none loaded.
    pub rules_line: fn(Option<&str>) -> String,
    /// The last successful refresh line: its age in seconds, or `None` and
    /// whether the latest run failed.
    pub refreshed_line: fn(Option<u64>, bool) -> String,
    /// The line saying the latest refresh failed.
    pub refresh_failed: &'static str,
    /// An age as words: `5 hours ago`.
    pub ago: fn(u64) -> String,
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Messages")
    }
}

/// English.
pub const EN: Messages = Messages {
    revision: |r| match r {
        Revision::Known {
            short,
            dirty: false,
        } => format!("commit {short}"),
        Revision::Known { short, dirty: true } => {
            format!("commit {short}, built with uncommitted changes")
        }
        Revision::Unknown => "commit unknown".to_owned(),
    },
    notice: |o| {
        format!(
            "{} — {}. Free software under the {} ({}, {}): you may run, study, share and modify it, \
             and anyone offering a modified version over a network must offer its source under the \
             same licence. The source code of this instance is at {} ({}).",
            o.program,
            o.copyright,
            o.license_name,
            o.license_spdx,
            o.license_url,
            o.repository,
            o.revision
        )
    },
    contact: |c| {
        let how = match c {
            Contact::Both { email, discord } => format!("{email}, or @{discord} on Discord"),
            Contact::Email(email) => email.to_owned(),
            Contact::Discord(discord) => format!("@{discord} on Discord"),
        };
        format!("For support, contact whoever runs this instance: {how}.")
    },
    rules_line: |date| format!("Comprehensive Rules: {}", date.unwrap_or("none loaded")),
    refreshed_line: |secs, failed| {
        let refreshed = match (secs, failed) {
            (Some(secs), _) => en_ago(secs),
            (None, false) => "no refresh run recorded yet".to_owned(),
            (None, true) => "none yet".to_owned(),
        };
        format!("Last successful refresh: {refreshed}")
    },
    refresh_failed: "Latest refresh: failed",
    ago: en_ago,
};

/// An age as English words: `under a minute ago`, `1 minute ago`, `5 hours
/// ago`, `3 days ago`. Minutes below an hour, hours below two days, days
/// after.
fn en_ago(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let (n, unit) = match secs {
        s if s < MINUTE => return "under a minute ago".to_owned(),
        s if s < HOUR => (s / MINUTE, "minute"),
        s if s < 2 * DAY => (s / HOUR, "hour"),
        s => (s / DAY, "day"),
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_matched_by_its_language_and_anything_else_falls_back() {
        assert_eq!(Locale::from_tag("en"), Some(Locale::En));
        assert_eq!(Locale::from_tag(" EN-gb "), Some(Locale::En));
        assert_eq!(Locale::from_tag("en_US"), Some(Locale::En));
        assert_eq!(Locale::from_tag("es-ES"), None);
        assert_eq!(Locale::from_tag(""), None);
        assert_eq!(
            Locale::negotiate(["fr", "de-AT", "en-US"], Locale::En),
            Locale::En
        );
        assert_eq!(Locale::negotiate([], Locale::En), Locale::En);
        assert_eq!(Locale::parse_setting(None), Ok(Locale::En));
        assert_eq!(Locale::parse_setting(Some(" ")), Ok(Locale::En));
        assert_eq!(Locale::parse_setting(Some("en-GB")), Ok(Locale::En));
        assert_eq!(
            Locale::parse_setting(Some("klingon")),
            Err("klingon".to_owned())
        );
    }

    #[test]
    fn ids_are_the_serde_names_and_every_locale_renders_every_message()
    -> Result<(), serde_json::Error> {
        for l in Locale::ALL {
            assert_eq!(serde_json::to_value(l)?, serde_json::json!(l.id()));
            let m = l.messages();
            for text in [
                (m.revision)(Revision::Known {
                    short: "a37d495",
                    dirty: true,
                }),
                (m.revision)(Revision::Unknown),
                (m.contact)(Contact::Both {
                    email: "a@b.c",
                    discord: "op",
                }),
                (m.rules_line)(None),
                (m.refreshed_line)(None, true),
                (m.ago)(90_000),
                m.refresh_failed.to_owned(),
            ] {
                assert!(!text.trim().is_empty(), "{l}");
            }
        }
        Ok(())
    }

    #[test]
    fn english_ages_read_in_the_largest_whole_unit_that_stays_precise() {
        for (secs, want) in [
            (0, "under a minute ago"),
            (59, "under a minute ago"),
            (60, "1 minute ago"),
            (3_599, "59 minutes ago"),
            (3_600, "1 hour ago"),
            (172_799, "47 hours ago"),
            (172_800, "2 days ago"),
        ] {
            assert_eq!((EN.ago)(secs), want, "{secs}");
        }
    }
}
