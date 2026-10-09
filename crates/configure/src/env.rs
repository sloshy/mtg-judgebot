//! The `.env` side: which variables exist, what the editor may show of
//! them, and a line-preserving writer.
//!
//! [`VARS`] lists every variable `.env.example` carries, each a
//! [`Kind::Secret`] or a [`Kind::Setting`]. Values reach the page only
//! through a [`Setting`] (made only from a `Kind::Setting` entry), so a secret
//! (an API key, `DISCORD_TOKEN`, `DATABASE_URL` with its password, the alert
//! webhook) is never read back: the page learns whether it is set. A
//! variable the registry does not know (a provider's `api_key_env`, `AWS_*`)
//! is treated as a secret, and so is a setting whose value is
//! [`Shown::Hidden`].
//!
//! Writing is two paths. A shown setting is changed with [`DotEnv::set`],
//! from a value the page displayed. Anything else is changed write-only with
//! [`DotEnv::replace`]: the page sends a new value for a [`Name`] and never
//! receives the old one, and no [`BadValue`] quotes a value.
//!
//! The help text is `.env.example`'s own comments ([`help`]): the block of
//! comment lines directly above a variable documents it, and consecutive
//! variables share the block. `registry_matches_the_example` holds the
//! registry and the example file to the same set of names.

use std::collections::BTreeMap;

use judge_bot::{
    alert::ALERT_WEBHOOK_ENV,
    budget::PERIOD_ENV,
    config::{ANTHROPIC_KEY_ENV, CONFIG_ENV, MAX_SPEND_ENV, VOYAGE_KEY_ENV},
    db::migrate::AUTO_MIGRATE_ENV,
    jobs::REFRESH_HOURS_ENV,
};
use judge_core::{
    operator::{OPERATOR_DISCORD_ENV, OPERATOR_EMAIL_ENV},
    source::SOURCE_URL_ENV,
};
use judgebot::roles::{API_INTERFACES_ENV, ROLES_ENV};
use serde::Serialize;

/// The annotated template, the source of every variable's help text.
pub const EXAMPLE: &str = include_str!("../../../.env.example");

/// Where a variable is shown in the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Group {
    /// Postgres.
    Database,
    /// What the `judgebot` process runs.
    Roles,
    /// The zero-config model setup and the `judge.toml` pointer.
    Models,
    /// The Discord bot.
    Discord,
    /// Spend, concurrency and the schema.
    Pipeline,
    /// The network roles.
    Http,
    /// `/mcp`.
    Mcp,
    /// Who runs the instance and where its source is.
    Instance,
    /// Compose and the image.
    Deployment,
}

/// How the editor offers a setting. Validation is never here: the draft is
/// run through the binaries' own parsers (`check`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "widget", rename_all = "lowercase")]
pub enum Widget {
    /// Free text.
    Text,
    /// An integer.
    Integer {
        /// The least value the reader accepts.
        min: u64,
    },
    /// A decimal number.
    Decimal,
    /// `true` / `false`.
    Toggle,
    /// One of a few values; blank is the default.
    Choice {
        /// The values.
        values: &'static [&'static str],
    },
}

/// Whether the editor may write a variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Never shown, never written: only whether it is set.
    Secret,
    /// Shown and written.
    Setting(Widget),
}

/// One variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Var {
    /// The name.
    pub name: &'static str,
    /// Where it is shown.
    pub group: Group,
    /// Secret or setting.
    pub kind: Kind,
}

const fn secret(name: &'static str, group: Group) -> Var {
    Var {
        name,
        group,
        kind: Kind::Secret,
    }
}

const fn setting(name: &'static str, group: Group, widget: Widget) -> Var {
    Var {
        name,
        group,
        kind: Kind::Setting(widget),
    }
}

const TEXT: Widget = Widget::Text;
const fn int(min: u64) -> Widget {
    Widget::Integer { min }
}

/// Every variable `.env.example` carries, in its order.
pub const VARS: &[Var] = &[
    secret("DATABASE_URL", Group::Database),
    setting("DB_PORT", Group::Database, int(1)),
    setting(ROLES_ENV, Group::Roles, TEXT),
    setting(API_INTERFACES_ENV, Group::Roles, TEXT),
    secret(ANTHROPIC_KEY_ENV, Group::Models),
    secret(VOYAGE_KEY_ENV, Group::Models),
    setting("ANTHROPIC_BASE_URL", Group::Models, TEXT),
    setting("VOYAGE_MODEL", Group::Models, TEXT),
    setting("VOYAGE_DIMENSIONS", Group::Models, int(1)),
    secret("DISCORD_TOKEN", Group::Discord),
    setting("JUDGE_ROLE", Group::Discord, TEXT),
    setting("GUILD_ID", Group::Discord, TEXT),
    setting(AUTO_MIGRATE_ENV, Group::Pipeline, Widget::Toggle),
    setting("JUDGE_CONCURRENCY", Group::Pipeline, int(1)),
    setting("JUDGE_USER_LIMIT", Group::Discord, int(0)),
    setting("JUDGE_USER_WINDOW_SECS", Group::Discord, int(1)),
    setting(MAX_SPEND_ENV, Group::Pipeline, Widget::Decimal),
    setting(
        PERIOD_ENV,
        Group::Pipeline,
        Widget::Choice {
            values: &["process", "day", "month"],
        },
    ),
    setting(REFRESH_HOURS_ENV, Group::Pipeline, int(0)),
    // A credential: whoever has the URL can post to the channel.
    secret(ALERT_WEBHOOK_ENV, Group::Pipeline),
    setting(CONFIG_ENV, Group::Models, TEXT),
    setting("API_ADDR", Group::Http, TEXT),
    setting("WEB_DIST", Group::Http, TEXT),
    setting("API_RATE_LIMIT", Group::Http, int(1)),
    setting("API_RATE_WINDOW_SECS", Group::Http, int(1)),
    setting(
        "API_CLIENT_IP",
        Group::Http,
        Widget::Choice {
            values: &["peer", "cloudflare"],
        },
    ),
    secret("MCP_TOKEN", Group::Mcp),
    setting("MCP_ALLOWED_HOSTS", Group::Mcp, TEXT),
    setting("MCP_JUDGE_LIMIT", Group::Mcp, int(1)),
    setting("MCP_JUDGE_WINDOW_SECS", Group::Mcp, int(1)),
    setting(SOURCE_URL_ENV, Group::Instance, TEXT),
    setting(OPERATOR_DISCORD_ENV, Group::Instance, TEXT),
    setting(OPERATOR_EMAIL_ENV, Group::Instance, TEXT),
    setting("COMPOSE_PROFILES", Group::Deployment, TEXT),
    setting("JUDGE_IMAGE", Group::Deployment, TEXT),
    setting("JUDGE_IMAGE_TAG", Group::Deployment, TEXT),
];

/// The registry entry for `name`.
#[must_use]
pub fn var(name: &str) -> Option<&'static Var> {
    VARS.iter().find(|v| v.name == name)
}

/// A variable the editor may write. Only [`Setting::named`] makes one, and
/// only for a [`Kind::Setting`] entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting(&'static Var);

impl Setting {
    /// The setting called `name`, if there is one: `None` for a secret or an
    /// unknown name.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        var(name)
            .filter(|v| matches!(v.kind, Kind::Setting(_)))
            .map(Self)
    }

    /// The name.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name
    }
}

/// Whether `line` assigns a variable, and which, as dotenvy reads it:
/// `NAME=...` with optional leading whitespace, `export`, and whitespace
/// around the `=`. The commented-out `#NAME=...` the example uses for an
/// optional variable counts too (`true`), spelled exactly so: `# NAME=`, a
/// space after the hash, is prose.
pub(crate) fn assignment(line: &str) -> Option<(&str, bool)> {
    if let Some(rest) = line.strip_prefix('#') {
        let (name, _) = rest.split_once('=')?;
        let mut bytes = name.bytes();
        let first = bytes.next()?;
        return (first.is_ascii_uppercase()
            && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
        .then_some((name, true));
    }
    let rest = line.trim_start();
    let rest = match rest.strip_prefix("export") {
        Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
        _ => rest,
    };
    let (name, _) = rest.split_once('=')?;
    let name = name.trim_end();
    let mut bytes = name.bytes();
    let first = bytes.next()?;
    ((first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
    .then_some((name, false))
}

/// Whether the value on an assignment line is substituted by the readers:
/// a `$` outside single quotes. Such a value can expand to anything,
/// another variable's secret included, so the editor never shows it.
fn expands(line: &str) -> bool {
    let Some((_, value)) = line.split_once('=') else {
        return false;
    };
    let value = value.trim();
    !(value.starts_with('\'') && value.ends_with('\'')) && value.contains('$')
}

/// Whether `value` is a URL carrying a user name or password.
fn has_userinfo(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| !u.username().is_empty() || u.password().is_some())
}

/// Each variable's help: the comment block directly above its line in
/// `text`, shared by consecutive variables.
#[must_use]
pub fn help(text: &str) -> BTreeMap<&str, String> {
    let mut out = BTreeMap::new();
    let mut block: Vec<&str> = vec![];
    let mut after_var = false;
    for line in text.lines() {
        if let Some((name, _)) = assignment(line) {
            out.entry(name).or_insert_with(|| block.join("\n"));
            after_var = true;
        } else if let Some(comment) = line.strip_prefix('#') {
            if after_var {
                block.clear();
                after_var = false;
            }
            let comment = comment.strip_prefix(' ').unwrap_or(comment);
            if !comment.starts_with("---") {
                block.push(comment);
            }
        } else {
            block.clear();
            after_var = false;
        }
    }
    out
}

/// A variable name the editor may write a replacement for: letters, digits
/// and `_`, not starting with a digit (what both dotenvy and Compose read).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name(String);

impl Name {
    /// `name`, if it is one.
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        let mut bytes = name.bytes();
        let first = bytes.next()?;
        ((first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .then(|| Self(name.to_owned()))
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A value the editor may write: one line, and spellable in a `.env` that
/// both `dotenvy` and Docker Compose read the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value(String);

/// Why a value cannot be written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BadValue {
    /// A line break would end the assignment.
    #[error("{0}: a value must be one line")]
    Multiline(String),
    /// Needs quoting, and holds a `'` along with a character double quotes
    /// do not protect.
    #[error("{0}: a value holding ' and any of \" \\ $ ` cannot be written to .env")]
    Unquotable(String),
    /// The written line would not read back as the value.
    #[error("{0}: the value would not read back unchanged from .env")]
    RoundTrip(String),
    /// The current value is not shown ([`Shown::Hidden`]), so it is not
    /// replaced either: the page never saw what it would overwrite.
    #[error("{0} is set to a value this editor does not show; replace it instead")]
    Hidden(String),
    /// A replacement must say something: a blank one is no replacement.
    #[error("{0}: an empty replacement; cancel it to keep the current value")]
    Empty(String),
}

impl Value {
    /// The value for `setting`, trimmed.
    ///
    /// # Errors
    /// [`BadValue`].
    pub fn new(setting: Setting, raw: &str) -> Result<Self, BadValue> {
        Self::named(setting.name(), raw)
    }

    /// A replacement for the variable `name` ([`DotEnv::replace`]): as
    /// [`Value::new`], and not blank.
    ///
    /// # Errors
    /// [`BadValue`]. No error carries the value: it may be a secret.
    pub fn replacement(name: &Name, raw: &str) -> Result<Self, BadValue> {
        if raw.trim().is_empty() {
            return Err(BadValue::Empty(name.0.clone()));
        }
        Self::named(&name.0, raw)
    }

    fn named(name: &str, raw: &str) -> Result<Self, BadValue> {
        let v = raw.trim();
        if v.contains(['\n', '\r', '\0']) {
            return Err(BadValue::Multiline(name.to_owned()));
        }
        let value = Self(v.to_owned());
        let line = value.line(name)?;
        // What both readers will see: dotenvy's reading of the line we write.
        let read = dotenvy::from_read_iter(line.as_bytes())
            .next()
            .and_then(Result::ok)
            .map(|(_, v)| v);
        if read.as_deref().unwrap_or_default() != v {
            return Err(BadValue::RoundTrip(name.to_owned()));
        }
        Ok(value)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `NAME=value`, quoted only when it must be. Single quotes are literal
    /// to both readers; `$` is quoted so Compose does not interpolate it.
    fn line(&self, name: &str) -> Result<String, BadValue> {
        let v = &self.0;
        let bare = v.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b',' | b'+' | b'%' | b'='
                )
        });
        Ok(if bare {
            format!("{name}={v}")
        } else if !v.contains('\'') {
            format!("{name}='{v}'")
        } else if !v.contains(['"', '\\', '$', '`']) {
            format!("{name}=\"{v}\"")
        } else {
            return Err(BadValue::Unquotable(name.to_owned()));
        })
    }
}

/// Why a `.env` cannot be edited as it stands.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BadEnv {
    /// dotenvy cannot read it, so neither can the binaries. dotenvy's own
    /// message quotes the line, which may hold a secret, so only the line
    /// number is kept.
    #[error(
        ".env{}: dotenvy cannot read it (an unquoted space, an unclosed quote?), so the binaries would refuse it too",
        line.map(|n| format!(" line {n}")).unwrap_or_default()
    )]
    Unreadable {
        /// The first line dotenvy refuses on its own, when one does.
        line: Option<usize>,
    },
    /// A variable is assigned more than once, so which one wins depends on
    /// the reader (dotenvy keeps the first, Compose the last).
    #[error(".env assigns {0} more than once; keep one line")]
    Duplicate(String),
    /// dotenvy reads an assignment this editor cannot place on a line.
    #[error(".env assigns {0} on a line this editor cannot rewrite; write it as {0}=value")]
    Unplaced(String),
}

/// What the editor may show of a setting's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown<'a> {
    /// Not assigned.
    Unset,
    /// The value.
    Value(&'a str),
    /// Assigned, but not shown: it expands `$` (and could expand to a
    /// secret), or it is a URL carrying credentials.
    Hidden,
}

/// A `.env` file: every line as it was, and what it assigns.
///
/// A value leaves only through [`DotEnv::setting`] (a [`Setting`]'s,
/// unless [`Shown::Hidden`]) or [`DotEnv::lookup`] (for the loaders, whose
/// errors name variables, not values); a secret's is never shown.
#[derive(Clone, Debug)]
pub struct DotEnv {
    lines: Vec<String>,
    values: BTreeMap<String, String>,
    interpolated: Vec<String>,
}

impl DotEnv {
    /// Parse `text` the way the binaries will: values from one dotenvy pass
    /// over the whole file (so `${A}` sees an earlier `A`), and every name
    /// dotenvy reads placed on exactly one line.
    ///
    /// # Errors
    /// [`BadEnv`].
    pub fn parse(text: &str) -> Result<Self, BadEnv> {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let mut placed = std::collections::BTreeSet::new();
        let mut interpolated = vec![];
        for line in &lines {
            let Some((name, false)) = assignment(line) else {
                continue;
            };
            if !placed.insert(name.to_owned()) {
                return Err(BadEnv::Duplicate(name.to_owned()));
            }
            if expands(line) {
                interpolated.push(name.to_owned());
            }
        }
        let mut values = BTreeMap::new();
        let unreadable = || BadEnv::Unreadable {
            line: lines
                .iter()
                .position(|l| dotenvy::from_read_iter(l.as_bytes()).any(|r| r.is_err()))
                .map(|i| i + 1),
        };
        for item in dotenvy::from_read_iter(text.as_bytes()) {
            let (name, value) = item.map_err(|_| unreadable())?;
            if !placed.contains(&name) {
                return Err(BadEnv::Unplaced(name));
            }
            values.insert(name, value);
        }
        Ok(Self {
            lines,
            values,
            interpolated,
        })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// What the page may see of `setting`.
    #[must_use]
    pub fn setting(&self, setting: Setting) -> Shown<'_> {
        let name = setting.name();
        match self.get(name) {
            None => Shown::Unset,
            Some(_) if self.interpolated.iter().any(|n| n == name) => Shown::Hidden,
            Some(v) if has_userinfo(v) => Shown::Hidden,
            Some(v) => Shown::Value(v),
        }
    }

    /// Every value the page must not see: secrets, variables the registry
    /// does not know, and hidden settings. Values under four characters are
    /// left out: masking them would garble every message for nothing.
    #[must_use]
    pub fn sensitive(&self) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(name, _)| {
                Setting::named(name).is_none_or(|s| self.setting(s) == Shown::Hidden)
            })
            .map(|(_, v)| v.trim())
            .filter(|v| v.len() >= 4)
            .collect()
    }

    /// Whether `name` is assigned a non-blank value.
    #[must_use]
    pub fn is_set(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.trim().is_empty())
    }

    /// The variables as the loaders read them. Secrets included: what reads
    /// this must not echo values.
    pub fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
        |k| self.get(k).map(str::to_owned)
    }

    /// Every assigned name.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// Set `setting` to `value`: rewrite its line, else uncomment the
    /// example's `#NAME=` line, else append. A blank value leaves a
    /// commented-out line commented. Re-parse the rendered text before
    /// reading values again: a `${NAME}` elsewhere would see the new one.
    ///
    /// # Errors
    /// [`BadValue`] when the value cannot be spelled.
    pub fn set(&mut self, setting: Setting, value: &Value) -> Result<(), BadValue> {
        let name = setting.name();
        match self.setting(setting) {
            Shown::Value(v) if v == value.as_str() => return Ok(()),
            Shown::Hidden => return Err(BadValue::Hidden(name.to_owned())),
            Shown::Value(_) | Shown::Unset => {}
        }
        self.place(name, value)
    }

    /// Write `value` to `name` without reading what was there: the way a
    /// secret, a hidden setting or any other variable is changed. The page
    /// sends the value and never gets it back.
    ///
    /// # Errors
    /// [`BadValue`] when the value cannot be spelled.
    pub fn replace(&mut self, name: &Name, value: &Value) -> Result<(), BadValue> {
        self.place(&name.0, value)
    }

    fn place(&mut self, name: &str, value: &Value) -> Result<(), BadValue> {
        let line = value.line(name)?;
        let at = |commented: bool| {
            self.lines
                .iter()
                .position(|l| assignment(l) == Some((name, commented)))
        };
        let target = match (at(false), at(true)) {
            (Some(i), _) => Some(i),
            (None, _) if value.as_str().is_empty() => return Ok(()),
            (None, commented) => commented,
        };
        let line_at = target.and_then(|i| self.lines.get_mut(i));
        match line_at {
            Some(l) => *l = line,
            None => self.lines.push(line),
        }
        self.interpolated.retain(|n| n != name);
        self.values
            .insert(name.to_owned(), value.as_str().to_owned());
        Ok(())
    }

    /// The file's text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn registry_matches_the_example() {
        let example: BTreeSet<&str> = EXAMPLE
            .lines()
            .filter_map(assignment)
            .map(|(n, _)| n)
            .collect();
        let registry: BTreeSet<&str> = VARS.iter().map(|v| v.name).collect();
        assert_eq!(example, registry);
        assert_eq!(registry.len(), VARS.len(), "a name listed twice");
        let help = help(EXAMPLE);
        for v in VARS {
            assert!(
                help.get(v.name).is_some_and(|h| !h.trim().is_empty()),
                "{} has no comment above it in .env.example",
                v.name
            );
        }
    }

    #[test]
    fn the_example_parses_as_a_dotenv() -> R {
        let env = DotEnv::parse(EXAMPLE)?;
        assert_eq!(
            env.setting(Setting::named("JUDGE_ROLE").ok_or("role")?),
            Shown::Value("Judge")
        );
        assert_eq!(
            env.setting(Setting::named("DB_PORT").ok_or("port")?),
            Shown::Unset,
            "commented out"
        );
        Ok(())
    }

    #[test]
    fn secrets_are_not_settings() {
        for name in [
            "ANTHROPIC_API_KEY",
            "DISCORD_TOKEN",
            "DATABASE_URL",
            "MCP_TOKEN",
            "JUDGE_ALERT_WEBHOOK",
            "LITELLM_KEY",
        ] {
            assert_eq!(Setting::named(name), None, "{name}");
        }
        assert!(Setting::named("JUDGE_MAX_USD").is_some());
    }

    #[test]
    fn help_is_the_block_above() {
        let h = help("# one\n# two\nA=1\nB=2\n# three\nC=3\n\nD=4\n# --- head\n# four\n#E=5\n");
        assert_eq!(h.get("A").map(String::as_str), Some("one\ntwo"));
        assert_eq!(h.get("B").map(String::as_str), Some("one\ntwo"));
        assert_eq!(h.get("C").map(String::as_str), Some("three"));
        assert_eq!(h.get("D").map(String::as_str), Some(""));
        assert_eq!(h.get("E").map(String::as_str), Some("four"));
    }

    #[test]
    fn set_rewrites_uncomments_or_appends_and_keeps_the_rest() -> R {
        let text = "# keep me\nJUDGE_MAX_USD=5\n#DB_PORT=5432\nSECRET='x y'\n";
        let mut env = DotEnv::parse(text)?;
        let s = |n| Setting::named(n).ok_or(n);
        let max = s("JUDGE_MAX_USD")?;
        env.set(max, &Value::new(max, "12.5")?)?;
        let port = s("DB_PORT")?;
        env.set(port, &Value::new(port, "5433")?)?;
        let hosts = s("MCP_ALLOWED_HOSTS")?;
        env.set(hosts, &Value::new(hosts, "a.example, localhost")?)?;
        let tag = s("JUDGE_IMAGE_TAG")?;
        env.set(tag, &Value::new(tag, "")?)?;
        assert_eq!(
            env.render(),
            "# keep me\nJUDGE_MAX_USD=12.5\nDB_PORT=5433\nSECRET='x y'\n\
             MCP_ALLOWED_HOSTS='a.example, localhost'\n"
        );
        let back = DotEnv::parse(&env.render())?;
        assert_eq!(back.setting(hosts), Shown::Value("a.example, localhost"));
        assert_eq!(back.lookup()("SECRET").as_deref(), Some("x y"));
        Ok(())
    }

    #[test]
    fn values_that_need_quoting_read_back() -> R {
        let s = Setting::named("JUDGE_ROLE").ok_or("role")?;
        for raw in [
            "Rules Judge",
            "it's",
            "$HOME",
            "a#b",
            "\"q\"",
            "x \"y\" 'z'",
        ] {
            match Value::new(s, raw) {
                Ok(v) => {
                    let mut env = DotEnv::parse("")?;
                    env.set(s, &v)?;
                    let back = DotEnv::parse(&env.render())?;
                    assert_eq!(back.setting(s), Shown::Value(raw), "{raw}");
                }
                Err(BadValue::Unquotable(_)) => assert!(raw.contains('\'') && raw.contains('"')),
                Err(e) => return Err(format!("{raw}: {e}").into()),
            }
        }
        assert!(Value::new(s, "a\nb").is_err());
        Ok(())
    }

    #[test]
    fn dotenvys_spellings_are_found_and_not_duplicated() -> R {
        let role = Setting::named("JUDGE_ROLE").ok_or("role")?;
        for text in [
            "  JUDGE_ROLE=x\n",
            "JUDGE_ROLE = x\n",
            "export  JUDGE_ROLE=x\n",
        ] {
            let mut env = DotEnv::parse(text)?;
            assert_eq!(env.setting(role), Shown::Value("x"), "{text:?}");
            env.set(role, &Value::new(role, "y")?)?;
            assert_eq!(env.render(), "JUDGE_ROLE=y\n", "{text:?}");
        }
        assert!(matches!(
            DotEnv::parse("A=1\n  A=2\n"),
            Err(BadEnv::Duplicate(_))
        ));
        Ok(())
    }

    #[test]
    fn substitution_is_whole_file_and_never_shown() -> R {
        let env = DotEnv::parse("SECRET=s3\nJUDGE_ROLE=${SECRET}\nJUDGE_IMAGE_TAG='$lit'\n")?;
        assert_eq!(env.lookup()("JUDGE_ROLE").as_deref(), Some("s3"));
        let s = |n| Setting::named(n).ok_or(n);
        assert_eq!(env.setting(s("JUDGE_ROLE")?), Shown::Hidden);
        assert_eq!(env.setting(s("JUDGE_IMAGE_TAG")?), Shown::Value("$lit"));
        let url = DotEnv::parse("ANTHROPIC_BASE_URL=https://u:p@proxy.example\n")?;
        assert_eq!(url.setting(s("ANTHROPIC_BASE_URL")?), Shown::Hidden);
        Ok(())
    }

    #[test]
    fn errors_and_sensitive_values_hold_no_secret() -> R {
        let err = DotEnv::parse("A=1\nDISCORD_TOKEN=SECRET with space\n")
            .err()
            .ok_or("parsed")?;
        assert_eq!(err, BadEnv::Unreadable { line: Some(2) });
        assert!(!err.to_string().contains("SECRET"));
        let env = DotEnv::parse(
            "CFG_TEST_TOKEN=tok-secret\nJUDGE_ROLE=${CFG_TEST_TOKEN}\nLITELLM_KEY=lk-secret\nJUDGE_IMAGE_TAG=1.1.0\n",
        )?;
        let mut sensitive = env.sensitive();
        sensitive.sort_unstable();
        assert_eq!(sensitive, ["lk-secret", "tok-secret", "tok-secret"]);
        let role = Setting::named("JUDGE_ROLE").ok_or("role")?;
        let mut env = env;
        assert_eq!(
            env.set(role, &Value::new(role, "x")?),
            Err(BadValue::Hidden("JUDGE_ROLE".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn a_replacement_overwrites_blind_and_never_quotes_its_value_in_errors() -> R {
        let mut env = DotEnv::parse(
            "DISCORD_TOKEN=old-token\n#LITELLM_KEY=\nJUDGE_ROLE=${CFG_TEST_TOKEN}\n",
        )?;
        let n = |s| Name::new(s).ok_or(s);
        for (name, value) in [
            ("DISCORD_TOKEN", "new token"),
            ("JUDGE_ROLE", "Judges"),
            ("OPENAI_API_KEY", "sk-x"),
        ] {
            env.replace(&n(name)?, &Value::replacement(&n(name)?, value)?)?;
        }
        let back = DotEnv::parse(&env.render())?;
        assert_eq!(back.lookup()("DISCORD_TOKEN").as_deref(), Some("new token"));
        assert_eq!(back.lookup()("JUDGE_ROLE").as_deref(), Some("Judges"));
        assert!(
            env.render().ends_with("OPENAI_API_KEY=sk-x\n"),
            "{}",
            env.render()
        );
        let err = Value::replacement(&n("X")?, "a\nSECRET")
            .err()
            .ok_or("accepted")?;
        assert!(!err.to_string().contains("SECRET"));
        assert_eq!(
            Value::replacement(&n("X")?, "  "),
            Err(BadValue::Empty("X".to_owned()))
        );
        assert!(Name::new("1A").is_none() && Name::new("A.B").is_none() && Name::new("").is_none());
        Ok(())
    }

    #[test]
    fn duplicates_are_refused() {
        assert!(matches!(
            DotEnv::parse("A=1\nA=2\n"),
            Err(BadEnv::Duplicate(n)) if n == "A"
        ));
    }
}
