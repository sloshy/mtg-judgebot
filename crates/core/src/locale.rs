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
//!
//! **The stub locale.** Every catalog also has a value for [`STUB_ID`], a
//! pseudo-locale whose every message is its own name in `⟦…⟧` with its
//! values, and each surface picks its catalog through [`Catalogs::new`],
//! which takes every locale's value. So a message added without its stub
//! value, or a surface that forgets a locale, does not compile, whichever
//! locales ship. The stub is a [`Locale`] variant only with this crate's
//! `stub-locale` feature, which only dev-dependencies turn on: the tests
//! negotiate it and render every message in it, and no build that ships can
//! choose it (D35).

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `JUDGE_LOCALE`: the instance's language when the asker's is not one of
/// [`Locale::ALL`].
pub const LOCALE_ENV: &str = "JUDGE_LOCALE";

/// The variables a terminal's message language is read from, in POSIX's
/// order of precedence ([`Locale::for_terminal`]).
pub const TERMINAL_ENV: [&str; 3] = ["LC_ALL", "LC_MESSAGES", "LANG"];

/// A POSIX locale value without its codeset and modifier:
/// `en_US.UTF-8@euro` is `en_US`.
fn terminal_language(value: &str) -> &str {
    value.trim().split(['.', '@']).next().unwrap_or_default()
}

/// The stub locale's id: `qps`, from ISO 639's private-use range, as the
/// pseudo-locales `qps-ploc` and the like are.
pub const STUB_ID: &str = "qps";

/// A language every surface has a catalog for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    /// English.
    #[default]
    En,
    /// The stub locale ([`STUB_ID`]): tests only (the module docs).
    #[cfg(feature = "stub-locale")]
    #[serde(rename = "qps")]
    Stub,
}

impl Locale {
    /// Every locale, in declaration order.
    #[cfg(not(feature = "stub-locale"))]
    pub const ALL: [Self; 1] = [Self::En];
    /// Every locale, in declaration order.
    #[cfg(feature = "stub-locale")]
    pub const ALL: [Self; 2] = [Self::En, Self::Stub];

    /// The id stored and sent (`calls.locale`, `JUDGE_LOCALE`, the web app's
    /// catalogs): the language subtag.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::En => "en",
            #[cfg(feature = "stub-locale")]
            Self::Stub => STUB_ID,
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

    /// The language a command line speaks: the terminal's message locale
    /// ([`TERMINAL_ENV`], the first one set decides, as POSIX says), when it
    /// names a locale, else `instance` (`JUDGE_LOCALE`). A value is read
    /// without its codeset and modifier (`en_US.UTF-8`, `en@quot`). `C` and
    /// `POSIX` (`C.UTF-8` too) name no language, so a variable holding one
    /// counts as unset and the next one is read: a container's `LC_ALL=C.UTF-8`
    /// says nothing about the person at the terminal.
    #[must_use]
    pub fn for_terminal(env: impl Fn(&str) -> Option<String>, instance: Self) -> Self {
        let tag = TERMINAL_ENV.into_iter().find_map(|var| {
            env(var)
                .map(|v| terminal_language(&v).to_owned())
                .filter(|l| !l.is_empty() && l != "C" && l != "POSIX")
        });
        Self::negotiate(tag.as_deref(), instance)
    }

    /// The language a command line speaks, from `env`:
    /// [`Self::for_terminal`] over `JUDGE_LOCALE`. A `JUDGE_LOCALE` with no
    /// catalog counts as unset here; the configuration refuses it when it
    /// loads. `judge-cli` and `judgebot` print their usage before `.env` is
    /// read, so this is the process environment's.
    #[must_use]
    pub fn for_command_line(env: impl Fn(&str) -> Option<String>) -> Self {
        let instance = Self::parse_setting(env(LOCALE_ENV).as_deref()).unwrap_or_default();
        Self::for_terminal(env, instance)
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
        CATALOGS.get(self)
    }
}

/// One surface's catalogs, a value per locale. [`Self::new`] takes every
/// locale's, the stub's included whether or not it can be chosen, so a new
/// [`Locale`] variant makes every surface's call stop compiling until it
/// names its catalog, and each catalog's struct literal lists the messages
/// it misses.
#[derive(Debug)]
pub struct Catalogs<T: ?Sized + 'static> {
    en: &'static T,
    #[cfg(feature = "stub-locale")]
    stub: &'static T,
}

impl<T: ?Sized + 'static> Catalogs<T> {
    /// Every locale's catalog: English, then the stub's.
    #[must_use]
    pub const fn new(en: &'static T, stub: &'static T) -> Self {
        #[cfg(not(feature = "stub-locale"))]
        let _ = stub;
        Self {
            en,
            #[cfg(feature = "stub-locale")]
            stub,
        }
    }

    /// The catalog for `locale`.
    #[must_use]
    pub const fn get(&self, locale: Locale) -> &'static T {
        match locale {
            Locale::En => self.en,
            #[cfg(feature = "stub-locale")]
            Locale::Stub => self.stub,
        }
    }
}

const CATALOGS: Catalogs<Messages> = Catalogs::new(&EN, &STUB);

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

/// The stub locale's: each message its name in `⟦…⟧`, with its values.
pub const STUB: Messages = Messages {
    revision: |r| match r {
        Revision::Known { short, dirty } => format!("⟦revision {short} {dirty}⟧"),
        Revision::Unknown => "⟦revision⟧".to_owned(),
    },
    notice: |o| {
        format!(
            "⟦notice {} {} {} {} {} {} {}⟧",
            o.program,
            o.copyright,
            o.license_name,
            o.license_spdx,
            o.license_url,
            o.repository,
            o.revision
        )
    },
    contact: |c| match c {
        Contact::Both { email, discord } => format!("⟦contact {email} {discord}⟧"),
        Contact::Email(email) => format!("⟦contact {email}⟧"),
        Contact::Discord(discord) => format!("⟦contact {discord}⟧"),
    },
    rules_line: |date| format!("⟦rules_line {}⟧", date.unwrap_or_default()),
    refreshed_line: |secs, failed| {
        format!("⟦refreshed_line {} {failed}⟧", secs.unwrap_or_default())
    },
    refresh_failed: "⟦refresh_failed⟧",
    ago: |secs| format!("⟦ago {secs}⟧"),
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
    fn a_terminal_speaks_its_message_locale_else_the_instances() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(Locale::for_terminal(env(&[]), Locale::En), Locale::En);
        assert_eq!(
            Locale::for_terminal(env(&[("LANG", "en_US.UTF-8")]), Locale::En),
            Locale::En
        );
        for c in ["C", "C.UTF-8", "POSIX", "fr_FR.UTF-8"] {
            let pairs: &'static [(&str, &str)] = match c {
                "C" => &[("LANG", "C")],
                "C.UTF-8" => &[("LC_ALL", "C.UTF-8")],
                "POSIX" => &[("LC_MESSAGES", "POSIX")],
                _ => &[("LANG", "fr_FR.UTF-8")],
            };
            assert_eq!(
                Locale::for_terminal(env(pairs), Locale::En),
                Locale::En,
                "{c}"
            );
        }
        assert_eq!(terminal_language(" en_US.UTF-8@euro "), "en_US");
        assert_eq!(terminal_language("en@quot"), "en");
        assert_eq!(terminal_language("C.UTF-8"), "C");
    }

    /// The stub locale is negotiated like any other: by the language
    /// subtag of an asker's tag, and as `JUDGE_LOCALE`.
    #[cfg(feature = "stub-locale")]
    #[test]
    fn the_stub_locale_is_negotiated_like_a_shipped_one() {
        assert_eq!(Locale::from_tag("qps-ploc"), Some(Locale::Stub));
        assert_eq!(
            Locale::negotiate(["fr", "qps", "en-US"], Locale::En),
            Locale::Stub
        );
        assert_eq!(Locale::negotiate(["fr"], Locale::Stub), Locale::Stub);
        assert_eq!(Locale::parse_setting(Some("qps")), Ok(Locale::Stub));
        assert_eq!(Locale::Stub.id(), STUB_ID);
        assert!(Locale::supported().contains(STUB_ID));
        assert_eq!((Locale::Stub.messages().ago)(5), "⟦ago 5⟧");
        let terminal = |pairs: [(&'static str, &'static str); 2]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        // The first variable set decides, a blank one is unset, and a
        // language with no catalog falls back to the instance's.
        assert_eq!(
            Locale::for_terminal(terminal([("LC_ALL", " "), ("LANG", "qps")]), Locale::En),
            Locale::Stub
        );
        assert_eq!(
            Locale::for_terminal(
                terminal([("LC_MESSAGES", "en_GB"), ("LANG", "qps")]),
                Locale::Stub
            ),
            Locale::En
        );
        assert_eq!(
            Locale::for_terminal(
                terminal([("LC_ALL", "fr_FR"), ("LANG", "en")]),
                Locale::Stub
            ),
            Locale::Stub
        );
        // A modifier is not part of the language; C and POSIX are no
        // language, so the next variable is read.
        assert_eq!(
            Locale::for_terminal(terminal([("LANG", "qps@quot"), ("LC_ALL", "")]), Locale::En),
            Locale::Stub
        );
        assert_eq!(
            Locale::for_terminal(
                terminal([("LC_MESSAGES", "en@quot"), ("LANG", "qps")]),
                Locale::Stub
            ),
            Locale::En
        );
        assert_eq!(
            Locale::for_terminal(
                terminal([("LC_ALL", "C.UTF-8"), ("LANG", "qps")]),
                Locale::En
            ),
            Locale::Stub
        );
        assert_eq!(
            Locale::for_terminal(
                terminal([("LC_ALL", "POSIX"), ("LC_MESSAGES", "C")]),
                Locale::Stub
            ),
            Locale::Stub
        );
        // A command line: the terminal's, then JUDGE_LOCALE (a bad one
        // counts as unset).
        assert_eq!(
            Locale::for_command_line(terminal([("JUDGE_LOCALE", "qps"), ("LANG", "C.UTF-8")])),
            Locale::Stub
        );
        assert_eq!(
            Locale::for_command_line(terminal([("JUDGE_LOCALE", "qps"), ("LANG", "en_US")])),
            Locale::En
        );
        assert_eq!(
            Locale::for_command_line(terminal([("JUDGE_LOCALE", "nope"), ("LANG", "qps")])),
            Locale::Stub
        );
        assert_eq!(
            Locale::for_command_line(terminal([("JUDGE_LOCALE", "nope"), ("LANG", "C")])),
            Locale::En
        );
    }

    /// The stub's catalog renders every message (it compiles in every
    /// build; the variant is what the feature adds).
    #[test]
    fn the_stub_catalog_renders_every_message() {
        assert_eq!((STUB.revision)(Revision::Unknown), "⟦revision⟧");
        assert_eq!(
            (STUB.rules_line)(Some("2026-08-19")),
            "⟦rules_line 2026-08-19⟧"
        );
        assert_eq!(STUB.refresh_failed, "⟦refresh_failed⟧");
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
