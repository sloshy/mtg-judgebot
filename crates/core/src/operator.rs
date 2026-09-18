//! Who runs this instance: the contact every remote interface names beside
//! the source offer, so a user with a problem knows whom to tell.
//!
//! Two facts, one per kind of surface. The Discord bot names a Discord
//! username ([`DiscordUsername`]); every other network surface (the HTTP API,
//! the web page, `/mcp`) names a support address ([`SupportEmail`]). Each is
//! *required* by the surface it belongs to, and that is a type, not a check:
//! the Discord layer takes a [`DiscordOperator`] and the HTTP layer a
//! [`NetworkOperator`], and the only way to either is through
//! [`Operator::for_discord`] / [`Operator::for_network`], which refuse when
//! the fact is missing. A surface shows the other contact too when it is set.
//!
//! This module is pure: reading the environment is the loader's job
//! (`judge_bot::config`).

use std::{borrow::Cow, sync::LazyLock};

use nutype::nutype;
use regex::Regex;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

/// The variable holding the operator's Discord username; required by the bot.
pub const OPERATOR_DISCORD_ENV: &str = "JUDGE_OPERATOR_DISCORD";
/// The variable holding the support address; required by `judge-api`.
pub const OPERATOR_EMAIL_ENV: &str = "JUDGE_OPERATOR_EMAIL";

/// Regex for [`DiscordUsername`]: Discord's own rule for usernames, 2 to 32
/// of lower-case letters, digits, `_` and `.`. (No two periods in a row is
/// checked beside it; the regex crate has no lookahead.)
pub const DISCORD_USERNAME_PATTERN: &str = r"^[a-z0-9_.]{2,32}$";

static DISCORD_USERNAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(DISCORD_USERNAME_PATTERN).expect("DISCORD_USERNAME_PATTERN is a valid regex")
});

/// A Discord username (not a display name, not a legacy `name#1234` tag). One
/// leading `@` is dropped and ASCII case is folded, as Discord does. Only
/// ASCII: a letter that merely lower-cases *into* `a-z` (the Kelvin sign) would
/// name somebody else, so it is left alone for the validator to refuse.
#[nutype(
    sanitize(with = |s: String| {
        let s = s.trim();
        s.strip_prefix('@').unwrap_or(s).to_ascii_lowercase()
    }),
    validate(predicate = is_discord_username),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct DiscordUsername(String);

fn is_discord_username(s: &str) -> bool {
    DISCORD_USERNAME_RE.is_match(s) && !s.contains("..")
}

impl JsonSchema for DiscordUsername {
    fn schema_name() -> Cow<'static, str> {
        "DiscordUsername".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "The Discord username of whoever runs this instance"
        })
    }
}

/// Regex for [`SupportEmail`]: one `@`, a local part of letters, digits and
/// `._+-`, and a dotted host name. Deliberately narrower than RFC 5322: the
/// address goes into a `mailto:` link, a Discord code span and plain text, so
/// anything that would need escaping in one of them (`?`, `&`, `%`, a
/// backtick) is refused rather than escaped three ways. (No leading,
/// trailing or doubled period in the local part is checked beside it.)
pub const SUPPORT_EMAIL_PATTERN: &str = r"^[A-Za-z0-9._+-]+@[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)+$";

/// The longest address SMTP carries (RFC 5321 §4.5.3.1.3).
const MAX_EMAIL_LEN: usize = 254;

static SUPPORT_EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(SUPPORT_EMAIL_PATTERN).expect("SUPPORT_EMAIL_PATTERN is a valid regex")
});

/// An address users can write to for support.
#[nutype(
    sanitize(trim),
    validate(predicate = is_support_email),
    derive(Clone, Debug, Display, Serialize, Deserialize, PartialEq, Eq, AsRef)
)]
pub struct SupportEmail(String);

fn is_support_email(s: &str) -> bool {
    let local = s.split('@').next().unwrap_or_default();
    s.len() <= MAX_EMAIL_LEN
        && SUPPORT_EMAIL_RE.is_match(s)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
}

impl JsonSchema for SupportEmail {
    fn schema_name() -> Cow<'static, str> {
        "SupportEmail".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "The support address of whoever runs this instance"
        })
    }
}

/// A surface was started without the contact it must name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MissingContact {
    /// The Discord bot without [`OPERATOR_DISCORD_ENV`].
    #[error(
        "{OPERATOR_DISCORD_ENV} is not set: the Discord bot must name the Discord username of \
         whoever runs it (shown by /help and /license)"
    )]
    Discord,
    /// A network surface without [`OPERATOR_EMAIL_ENV`].
    #[error(
        "{OPERATOR_EMAIL_ENV} is not set: the HTTP API, the web page and /mcp must name a support \
         email address for whoever runs them (shown by GET /api/about, the page footer and the MCP \
         instructions)"
    )]
    Email,
}

/// What an instance says about who runs it. Either fact may be absent here:
/// this is what a local process (`judge-cli`, `judge-mcp` on stdio) holds,
/// where the operator and the user are the same person.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Operator {
    discord: Option<DiscordUsername>,
    email: Option<SupportEmail>,
}

impl Operator {
    /// An operator with these contacts.
    #[must_use]
    pub fn new(discord: Option<DiscordUsername>, email: Option<SupportEmail>) -> Self {
        Self { discord, email }
    }

    /// The Discord username, when set.
    #[must_use]
    pub fn discord(&self) -> Option<&DiscordUsername> {
        self.discord.as_ref()
    }

    /// The support address, when set.
    #[must_use]
    pub fn email(&self) -> Option<&SupportEmail> {
        self.email.as_ref()
    }

    /// The contact as one plain-text sentence, or `None` when there is
    /// nothing to say.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        let how = match (&self.email, &self.discord) {
            (Some(email), Some(discord)) => format!("{email}, or @{discord} on Discord"),
            (Some(email), None) => email.to_string(),
            (None, Some(discord)) => format!("@{discord} on Discord"),
            (None, None) => return None,
        };
        Some(format!(
            "For support, contact whoever runs this instance: {how}."
        ))
    }

    /// The operator as the Discord bot must know them.
    ///
    /// # Errors
    /// [`MissingContact::Discord`] without a Discord username.
    pub fn for_discord(self) -> Result<DiscordOperator, MissingContact> {
        match self.discord.clone() {
            Some(username) => Ok(DiscordOperator {
                username,
                operator: self,
            }),
            None => Err(MissingContact::Discord),
        }
    }

    /// The operator as a network surface must know them.
    ///
    /// # Errors
    /// [`MissingContact::Email`] without a support address.
    pub fn for_network(self) -> Result<NetworkOperator, MissingContact> {
        match self.email.clone() {
            Some(email) => Ok(NetworkOperator {
                email,
                operator: self,
            }),
            None => Err(MissingContact::Email),
        }
    }
}

/// An [`Operator`] known to have a Discord username. Only
/// [`Operator::for_discord`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordOperator {
    username: DiscordUsername,
    operator: Operator,
}

impl DiscordOperator {
    /// The username `/help` and `/license` name.
    #[must_use]
    pub fn username(&self) -> &DiscordUsername {
        &self.username
    }

    /// Every contact, this one included.
    #[must_use]
    pub fn operator(&self) -> &Operator {
        &self.operator
    }
}

/// An [`Operator`] known to have a support address. Only
/// [`Operator::for_network`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkOperator {
    email: SupportEmail,
    operator: Operator,
}

impl NetworkOperator {
    /// The address `GET /api/about`, the page and `/mcp` name.
    #[must_use]
    pub fn email(&self) -> &SupportEmail {
        &self.email
    }

    /// Every contact, this one included.
    #[must_use]
    pub fn operator(&self) -> &Operator {
        &self.operator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = anyhow::Result<()>;

    #[test]
    fn discord_usernames_follow_discords_rule() -> R {
        assert_eq!(
            DiscordUsername::try_new("  @Some.Judge_42 ")?.as_ref(),
            "some.judge_42"
        );
        for bad in [
            "",
            "@",
            "a",
            "@@somejudge",
            "some\u{212A}judge",
            "two..dots",
            "has space",
            "legacy#1234",
            "display-name",
            "thirty-three-characters-is-too-long",
            "abcdefghijklmnopqrstuvwxyz0123456",
            "<@123456789012345678>",
        ] {
            assert!(DiscordUsername::try_new(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn support_emails_are_plain_addresses() -> R {
        assert_eq!(
            SupportEmail::try_new(" judge+help@example.org ")?.as_ref(),
            "judge+help@example.org"
        );
        let long = format!("{}@example.org", "a".repeat(250));
        for bad in [
            "",
            "judge",
            "judge@",
            "@example.org",
            "judge@localhost",
            "judge@example..org",
            "judge@-example.org",
            ".judge@example.org",
            "judge.@example.org",
            "ju..dge@example.org",
            "a b@example.org",
            "Judge <judge@example.org>",
            "mailto:judge@example.org",
            "judge@example.org\nBcc: x@example.org",
            "judge@example.org?subject=x",
            "ju`dge@example.org",
            "ju%40dge@example.org",
            long.as_str(),
        ] {
            assert!(SupportEmail::try_new(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn each_surface_refuses_without_its_own_contact() -> R {
        let discord = DiscordUsername::try_new("somejudge")?;
        let email = SupportEmail::try_new("judge@example.org")?;

        let nobody = Operator::default();
        assert_eq!(nobody.notice(), None);
        assert_eq!(nobody.clone().for_discord(), Err(MissingContact::Discord));
        assert_eq!(nobody.for_network(), Err(MissingContact::Email));

        let only_discord = Operator::new(Some(discord.clone()), None);
        assert_eq!(
            only_discord.clone().for_network(),
            Err(MissingContact::Email)
        );
        assert_eq!(only_discord.for_discord()?.username(), &discord);

        let only_email = Operator::new(None, Some(email.clone()));
        assert_eq!(
            only_email.clone().for_discord(),
            Err(MissingContact::Discord)
        );
        assert_eq!(only_email.for_network()?.email(), &email);

        let both = Operator::new(Some(discord), Some(email));
        let notice = both.notice().unwrap_or_default();
        assert!(notice.contains("judge@example.org"), "{notice}");
        assert!(notice.contains("@somejudge on Discord"), "{notice}");
        assert_eq!(both.clone().for_network()?.operator(), &both);
        Ok(())
    }
}
