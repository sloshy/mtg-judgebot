//! judge-config's text, one [`Messages`] per [`Locale`] (`judge_core::locale`,
//! D35): what it prints in the terminal, what the server says to the page
//! (refusals, a draft's problems, the checks' own lines, warnings), and the
//! help overrides a locale gives the English help sources.
//!
//! The page's own text (labels, buttons, notes) is `ui/messages.js`, one
//! object per locale; [`tests`] holds every object there to English's keys.
//!
//! Quoted as they come, in English: the loaders' refusals (`ConfigError`,
//! the roles' and the API's problems, `Config::summary`), which are the
//! binaries' own startup diagnostics, the TOML parser's and the operating
//! system's errors.
//!
//! **Help.** The English help is the loader's doc comments (the schema,
//! `judge_bot::config::file_schema`) and `.env.example`'s comment blocks, so
//! it is never restated here. Another locale translates it with
//! [`Messages::schema_help`] (keyed by the JSON pointer of the schema object
//! whose `description` it replaces) and [`Messages::env_help`] (keyed by
//! variable name); an entry it leaves out stays English. A test holds every
//! key to a real schema object or registry variable, so a renamed knob
//! fails it.
//!
//! A [`BadValue`] carries only a variable's name, so no locale's text for it
//! can quote a value.

use std::fmt;

use judge_core::{Locale, locale::Catalogs};
use judgebot::roles::ComposeOrigin;

use crate::{
    env::{BadEnv, BadValue},
    toml_doc::BadDraft,
};

/// A role whose surface is not checked because the roles leave it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absent {
    /// `--admin`.
    Admin,
    /// `--discord`.
    Discord,
    /// `--api`, `--web` and `--mcp`, all three.
    Network,
}

/// One locale's judge-config text.
#[derive(Clone, Copy)]
pub struct Messages {
    // ---- the terminal ----
    /// `judge-config --help`.
    pub usage: &'static str,
    /// An option given without its value, given the option.
    pub needs_value: fn(&str) -> String,
    /// `--listen` is not an address.
    pub bad_listen: &'static str,
    /// An argument judge-config does not take, given it.
    pub unknown_argument: fn(&str) -> String,
    /// The files edited: the `.env`'s path and the `judge.toml`'s.
    pub editing: fn(&str, &str) -> String,
    /// The address to open, with its token.
    pub open: fn(&str) -> String,
    /// How to stop it.
    pub stop: &'static str,
    /// Listening beyond loopback: the address and the port.
    pub not_loopback: fn(&str, u16) -> String,

    // ---- requests refused ----
    /// A `Host` that is not a loopback name or an `--allow-host` one.
    pub wrong_host: &'static str,
    /// A request without the token, or with another.
    pub wrong_token: &'static str,
    /// A save after either file changed on disk.
    pub changed_on_disk: &'static str,
    /// After a failed write, which files were saved before it.
    pub saved_before: fn(&str) -> String,

    // ---- a draft refused ----
    /// A draft without a file while the file exists, given its path.
    pub never_deletes: fn(&str) -> String,
    /// The file on disk is not TOML, so the form cannot merge into it:
    /// the parser's error.
    pub not_toml_on_disk: fn(&str) -> String,
    /// The form's JSON cannot be written as TOML.
    pub bad_draft: fn(&BadDraft) -> String,
    /// A shown value for a variable that is no setting, given its name.
    pub not_a_setting: fn(&str) -> String,
    /// A replacement for a name that is no variable name, given it.
    pub not_a_name: fn(&str) -> String,
    /// A replacement for a shown setting, given its name.
    pub shown_setting: fn(&str) -> String,
    /// A value that cannot be written.
    pub bad_value: fn(&BadValue) -> String,
    /// A `.env` that cannot be edited as it stands.
    pub bad_env: fn(&BadEnv) -> String,
    /// The review's line for a variable replaced write-only, given its name.
    pub replaced_line: fn(&str) -> String,

    // ---- warnings about the files together ----
    /// `JUDGE_CONFIG` blank while a `judge.toml` is written: its path, and
    /// the value that names it.
    pub judge_config_blank: fn(&str, &str) -> String,
    /// `JUDGE_CONFIG` names another file: its value, and the one edited.
    pub judge_config_other: fn(&str, &str) -> String,
    /// `DB_PORT` and `DATABASE_URL`'s port disagree: the published port and
    /// the URL's.
    pub db_port_mismatch: fn(&str, u16) -> String,

    // ---- the checks' own lines ----
    /// The roles the compose service runs, and where they come from.
    pub roles_from: fn(&str, ComposeOrigin) -> String,
    /// `--admin` passes: where the container listens and where the proxy
    /// publishes it.
    pub admin_listens: fn(&str, &str) -> String,
    /// The network roles pass, given their flags.
    pub serving: fn(&str) -> String,
    /// A role's surface is not checked: the roles do not parse.
    pub roles_unparsed: &'static str,
    /// A role's surface is not checked: the roles leave it out, given them.
    pub not_among_roles: fn(Absent, &str) -> String,
    /// A role's own settings pass, but the models do not load.
    pub models_fail: &'static str,
    /// `DB_PORT` is not a port, given it (a setting, shown on the page).
    pub bad_db_port: fn(&str) -> String,

    // ---- help ----
    /// Schema help in this locale: (the JSON pointer of the schema object
    /// whose `description` it replaces, the text). Empty for English, whose
    /// help is the doc comments.
    pub schema_help: &'static [(&'static str, &'static str)],
    /// `.env` help in this locale: (variable, text). Empty for English,
    /// whose help is `.env.example`'s comments.
    pub env_help: &'static [(&'static str, &'static str)],
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("configure::catalog::Messages")
    }
}

const CATALOGS: Catalogs<Messages> = Catalogs::new(&EN, &STUB);

/// judge-config's text in `locale`.
#[must_use]
pub const fn messages(locale: Locale) -> &'static Messages {
    CATALOGS.get(locale)
}

/// English.
pub const EN: Messages = Messages {
    usage: "\
usage: judge-config [--env FILE] [--config FILE] [--listen ADDR] [--allow-host NAME]...

A page on localhost for editing judge.toml and .env, each draft checked by
the binaries' own loaders. Secrets in .env are never shown: the page says
whether each is set, and can replace one with a value you type.

  --env FILE         the .env to edit (default ./.env)
  --config FILE      the judge.toml to edit (default: JUDGE_CONFIG from the
                     .env, else judge.toml beside it)
  --listen ADDR      where to listen (default 127.0.0.1:8790)
  --allow-host NAME  accept this Host header too (loopback names always are)",
    needs_value: |arg| format!("{arg} needs a value"),
    bad_listen: "--listen must be an address such as 127.0.0.1:8790",
    unknown_argument: |arg| format!("unknown argument {arg:?}"),
    editing: |env, toml| format!("editing {env} and {toml}"),
    open: |url| format!("open {url}"),
    stop: "Ctrl-C to stop.",
    not_loopback: |addr, port| {
        format!(
            "judge-config: listening on {addr}, not loopback. Publish the port on 127.0.0.1 only \
             (docker run -p 127.0.0.1:{port}:{port}) or anyone who can reach it and learns the \
             token can edit your settings."
        )
    },

    wrong_host: "judge-config answers on localhost only (pass --allow-host NAME to add one)",
    wrong_token: "missing or wrong token: open the URL judge-config printed",
    changed_on_disk: "a file changed on disk since this page read it; reload to see it",
    saved_before: |saved| format!(" ({saved} was saved)"),

    never_deletes: |path| {
        format!(
            "{path} exists; this editor never deletes it (remove it yourself for the zero-config \
             setup)"
        )
    },
    not_toml_on_disk: |e| format!("the file on disk is not TOML ({e}); edit it as text"),
    bad_draft: |d| match d {
        BadDraft::NotATable => "the draft must be a JSON object".to_owned(),
        BadDraft::Number(path) => format!("{path}: not a TOML number"),
    },
    not_a_setting: |name| format!("{name} is not a setting this editor writes"),
    not_a_name: |name| format!("{name:?} is not a variable name (letters, digits, _)"),
    shown_setting: |name| format!("{name} is shown: change it in its field"),
    bad_value: |b| match b {
        BadValue::Multiline(n) => format!("{n}: a value must be one line"),
        BadValue::Unquotable(n) => {
            format!("{n}: a value holding ' and any of \" \\ $ ` cannot be written to .env")
        }
        BadValue::RoundTrip(n) => format!("{n}: the value would not read back unchanged from .env"),
        BadValue::Hidden(n) => {
            format!("{n} is set to a value this editor does not show; replace it instead")
        }
        BadValue::Empty(n) => {
            format!("{n}: an empty replacement; cancel it to keep the current value")
        }
    },
    bad_env: |b| match b {
        BadEnv::Unreadable { line } => format!(
            ".env{}: dotenvy cannot read it (an unquoted space, an unclosed quote?), so the \
             binaries would refuse it too",
            line.map(|n| format!(" line {n}")).unwrap_or_default()
        ),
        BadEnv::Duplicate(n) => format!(".env assigns {n} more than once; keep one line"),
        BadEnv::Unplaced(n) => {
            format!(".env assigns {n} on a line this editor cannot rewrite; write it as {n}=value")
        }
    },
    replaced_line: |name| format!("{name}: new value (not shown)"),

    judge_config_blank: |path, spelled| {
        format!(
            "JUDGE_CONFIG is blank, so the containers will not read {path} (cargo run will). Set \
             JUDGE_CONFIG={spelled} for Docker."
        )
    },
    judge_config_other: |set, spelled| {
        format!("JUDGE_CONFIG={set} names another file than the one edited here ({spelled}).")
    },
    db_port_mismatch: |published, url| {
        format!(
            "DB_PORT publishes Postgres on {published}, but DATABASE_URL points at port {url}: \
             change the port in DATABASE_URL too (this editor does not write it)."
        )
    },

    roles_from: |flags, origin| {
        let from = match origin {
            ComposeOrigin::Roles => judgebot::roles::ROLES_ENV,
            ComposeOrigin::Default => "the compose file's default",
        };
        format!("{flags} (from {from})")
    },
    admin_listens: |container, published| {
        format!("the container listens on {container}; the proxy publishes it on {published}")
    },
    serving: |flags| format!("serving {flags}"),
    roles_unparsed: "the roles do not parse",
    not_among_roles: |absent, flags| {
        let what = match absent {
            Absent::Admin => "--admin is not",
            Absent::Discord => "--discord is not",
            Absent::Network => "none of --api, --web and --mcp is",
        };
        format!("{what} among the roles ({flags})")
    },
    models_fail: "its own settings pass; the model configuration does not load",
    bad_db_port: |port| format!("DB_PORT must be a port number 1..=65535, got {port:?}"),

    schema_help: &[],
    env_help: &[],
};

/// The stub locale's (`judge_core::locale`, tests only): each message its
/// name in `⟦…⟧`, with its values, and one help override of each kind.
pub const STUB: Messages = Messages {
    usage: "⟦usage⟧",
    needs_value: |arg| format!("⟦needs_value {arg}⟧"),
    bad_listen: "⟦bad_listen⟧",
    unknown_argument: |arg| format!("⟦unknown_argument {arg}⟧"),
    editing: |env, toml| format!("⟦editing {env} {toml}⟧"),
    open: |url| format!("⟦open⟧ {url}"),
    stop: "⟦stop⟧",
    not_loopback: |addr, port| format!("⟦not_loopback {addr} {port}⟧"),

    wrong_host: "⟦wrong_host⟧",
    wrong_token: "⟦wrong_token⟧",
    changed_on_disk: "⟦changed_on_disk⟧",
    saved_before: |saved| format!(" ⟦saved_before {saved}⟧"),

    never_deletes: |path| format!("⟦never_deletes {path}⟧"),
    not_toml_on_disk: |e| format!("⟦not_toml_on_disk {e}⟧"),
    bad_draft: |d| match d {
        BadDraft::NotATable => "⟦bad_draft⟧".to_owned(),
        BadDraft::Number(path) => format!("⟦bad_draft {path}⟧"),
    },
    not_a_setting: |name| format!("⟦not_a_setting {name}⟧"),
    not_a_name: |name| format!("⟦not_a_name {name}⟧"),
    shown_setting: |name| format!("⟦shown_setting {name}⟧"),
    bad_value: |b| match b {
        BadValue::Multiline(n) => format!("⟦bad_value multiline {n}⟧"),
        BadValue::Unquotable(n) => format!("⟦bad_value unquotable {n}⟧"),
        BadValue::RoundTrip(n) => format!("⟦bad_value round_trip {n}⟧"),
        BadValue::Hidden(n) => format!("⟦bad_value hidden {n}⟧"),
        BadValue::Empty(n) => format!("⟦bad_value empty {n}⟧"),
    },
    bad_env: |b| match b {
        BadEnv::Unreadable { line } => {
            format!("⟦bad_env unreadable {}⟧", line.unwrap_or_default())
        }
        BadEnv::Duplicate(n) => format!("⟦bad_env duplicate {n}⟧"),
        BadEnv::Unplaced(n) => format!("⟦bad_env unplaced {n}⟧"),
    },
    replaced_line: |name| format!("⟦replaced_line {name}⟧"),

    judge_config_blank: |path, spelled| format!("⟦judge_config_blank {path} {spelled}⟧"),
    judge_config_other: |set, spelled| format!("⟦judge_config_other {set} {spelled}⟧"),
    db_port_mismatch: |published, url| format!("⟦db_port_mismatch {published} {url}⟧"),

    roles_from: |flags, origin| format!("⟦roles_from {flags} {origin:?}⟧"),
    admin_listens: |container, published| format!("⟦admin_listens {container} {published}⟧"),
    serving: |flags| format!("⟦serving {flags}⟧"),
    roles_unparsed: "⟦roles_unparsed⟧",
    not_among_roles: |absent, flags| format!("⟦not_among_roles {absent:?} {flags}⟧"),
    models_fail: "⟦models_fail⟧",
    bad_db_port: |port| format!("⟦bad_db_port {port}⟧"),

    schema_help: &[("/properties/providers", "⟦schema_help providers⟧")],
    env_help: &[("JUDGE_LOCALE", "⟦env_help JUDGE_LOCALE⟧")],
};

/// `.env` help in `locale`: `english` (`.env.example`'s block for `name`)
/// unless the locale translates it.
#[must_use]
pub fn env_help<'a>(locale: Locale, name: &str, english: Option<&'a str>) -> Option<&'a str> {
    messages(locale)
        .env_help
        .iter()
        .find(|(var, _)| *var == name)
        .map(|(_, text)| *text)
        .or(english)
}

/// `schema` with `locale`'s help in place of the English doc comments it
/// translates.
pub fn localize_schema(locale: Locale, schema: &mut serde_json::Value) {
    for (pointer, text) in messages(locale).schema_help {
        if let Some(serde_json::Value::Object(o)) = schema.pointer_mut(pointer) {
            o.insert("description".to_owned(), serde_json::json!(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::VARS;

    /// Every locale renders every message.
    #[test]
    fn every_locale_renders_every_message() {
        for l in Locale::ALL {
            let m = messages(l);
            let mut texts = vec![
                m.usage.to_owned(),
                (m.needs_value)("--env"),
                m.bad_listen.to_owned(),
                (m.unknown_argument)("--x"),
                (m.editing)("/a/.env", "/a/judge.toml"),
                (m.open)("http://127.0.0.1:8790/#token=t"),
                m.stop.to_owned(),
                (m.not_loopback)("0.0.0.0:8790", 8790),
                m.wrong_host.to_owned(),
                m.wrong_token.to_owned(),
                m.changed_on_disk.to_owned(),
                (m.saved_before)("/a/judge.toml"),
                (m.never_deletes)("/a/judge.toml"),
                (m.not_toml_on_disk)("line 1"),
                (m.bad_draft)(&BadDraft::NotATable),
                (m.bad_draft)(&BadDraft::Number("a.b".to_owned())),
                (m.not_a_setting)("X"),
                (m.not_a_name)("X Y"),
                (m.shown_setting)("X"),
                (m.bad_env)(&BadEnv::Unreadable { line: Some(2) }),
                (m.bad_env)(&BadEnv::Duplicate("X".to_owned())),
                (m.bad_env)(&BadEnv::Unplaced("X".to_owned())),
                (m.replaced_line)("X"),
                (m.judge_config_blank)("/a/judge.toml", "./judge.toml"),
                (m.judge_config_other)("./x.toml", "./judge.toml"),
                (m.db_port_mismatch)("5433", 5432),
                (m.roles_from)("--api", ComposeOrigin::Roles),
                (m.roles_from)("--api", ComposeOrigin::Default),
                (m.admin_listens)("0.0.0.0:8789", "127.0.0.1:8789"),
                (m.serving)("--api --web"),
                m.roles_unparsed.to_owned(),
                m.models_fail.to_owned(),
                (m.bad_db_port)("x"),
            ];
            for a in [Absent::Admin, Absent::Discord, Absent::Network] {
                texts.push((m.not_among_roles)(a, "--jobs"));
            }
            for t in &texts {
                assert!(!t.trim().is_empty(), "{l}: an empty message");
            }
        }
    }

    /// A [`BadValue`] holds a name and nothing else, and every locale's text
    /// for it names the variable.
    #[test]
    fn a_bad_value_names_its_variable_in_every_locale() {
        for l in Locale::ALL {
            for b in [
                BadValue::Multiline("NAME_X".to_owned()),
                BadValue::Unquotable("NAME_X".to_owned()),
                BadValue::RoundTrip("NAME_X".to_owned()),
                BadValue::Hidden("NAME_X".to_owned()),
                BadValue::Empty("NAME_X".to_owned()),
            ] {
                assert!((messages(l).bad_value)(&b).contains("NAME_X"), "{l}: {b:?}");
            }
        }
    }

    /// Every help override names a schema object with a description, or a
    /// variable the registry and `.env.example` document.
    #[test]
    fn every_help_override_names_a_real_knob() {
        let schema = judge_bot::config::file_schema();
        let example = crate::env::help(crate::env::EXAMPLE);
        for l in Locale::ALL {
            let m = messages(l);
            for (pointer, _) in m.schema_help {
                assert!(
                    schema
                        .pointer(pointer)
                        .and_then(|o| o.get("description"))
                        .is_some(),
                    "{l}: no schema description at {pointer}"
                );
            }
            for (var, _) in m.env_help {
                assert!(
                    VARS.iter().any(|v| v.name == *var) && example.contains_key(var),
                    "{l}: {var} is not a documented variable"
                );
            }
        }
    }

    /// A locale's override replaces the English help; an entry it leaves out
    /// stays English.
    #[test]
    fn help_falls_back_to_english_per_entry() {
        let schema = judge_bot::config::file_schema();
        let english = |p: &str| {
            schema
                .pointer(p)
                .and_then(|o| o.get("description"))
                .cloned()
        };
        let mut stub = schema.clone();
        localize_schema(Locale::Stub, &mut stub);
        let providers = "/properties/providers/description";
        assert_eq!(
            stub.pointer(providers),
            Some(&serde_json::json!("⟦schema_help providers⟧"))
        );
        let models = "/properties/models";
        assert_eq!(
            stub.pointer(models)
                .and_then(|o| o.get("description"))
                .cloned(),
            english(models)
        );
        let mut en = schema.clone();
        localize_schema(Locale::En, &mut en);
        assert_eq!(en, schema);
        assert_eq!(
            env_help(Locale::Stub, "JUDGE_LOCALE", Some("English")),
            Some("⟦env_help JUDGE_LOCALE⟧")
        );
        assert_eq!(
            env_help(Locale::Stub, "JUDGE_ROLE", Some("English")),
            Some("English")
        );
        assert_eq!(
            env_help(Locale::En, "JUDGE_LOCALE", Some("English")),
            Some("English")
        );
    }

    /// The keys each locale object in `ui/messages.js` names.
    ///
    /// The file's format (documented at its top): each locale is a
    /// `const <ID> = {` line, its messages one per line as `  key:` at two
    /// spaces, closed by `};` alone on a line; then `export const MESSAGES
    /// = { <id>: <ID>, … };` lists every one.
    ///
    /// A message that is an object of messages (`surfaces: {` alone on its
    /// line) names its own keys two spaces deeper, as `surfaces.http`.
    fn ui_catalogs(source: &str) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = vec![];
        let mut current: Option<(String, Vec<String>)> = None;
        // The enclosing objects' keys, each with its indent.
        let mut nest: Vec<(usize, String)> = vec![];
        for line in source.lines() {
            if let Some(name) = line
                .strip_prefix("const ")
                .and_then(|l| l.strip_suffix(" = {"))
            {
                current = Some((name.to_owned(), vec![]));
                nest.clear();
            } else if line == "};" {
                if let Some(done) = current.take() {
                    out.push(done);
                }
            } else if let Some((_, keys)) = current.as_mut() {
                let rest = line.trim_start();
                let indent = line.len() - rest.len();
                while nest.last().is_some_and(|(i, _)| *i >= indent) {
                    nest.pop();
                }
                let depth = nest.len();
                if indent != 2 * (depth + 1) {
                    continue;
                }
                let Some((key, value)) = rest.split_once(':') else {
                    continue;
                };
                if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    continue;
                }
                let path: Vec<&str> = nest.iter().map(|(_, k)| k.as_str()).chain([key]).collect();
                keys.push(path.join("."));
                if value.trim() == "{" {
                    nest.push((indent, key.to_owned()));
                }
            }
        }
        out
    }

    /// The page's catalogs: every locale object names exactly English's
    /// messages, and `MESSAGES` lists every object under a locale id.
    #[test]
    fn every_page_catalog_names_the_english_messages() {
        let source = include_str!("../ui/messages.js");
        let catalogs = ui_catalogs(source);
        let english = catalogs
            .iter()
            .find(|(name, _)| name == "EN")
            .map(|(_, keys)| keys.clone())
            .unwrap_or_default();
        assert!(
            english.len() > 50,
            "English names {} messages",
            english.len()
        );
        let mut sorted = english.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), english.len(), "a key named twice");
        for (name, keys) in &catalogs {
            let mut keys = keys.clone();
            keys.sort();
            assert_eq!(keys, sorted, "{name} names other messages than EN");
            let id = name.to_ascii_lowercase();
            assert!(
                source.contains(&format!("  {id}: {name},")),
                "MESSAGES does not list {name} as {id}"
            );
            assert!(
                Locale::ALL.iter().any(|l| l.id() == id),
                "{name} is no locale"
            );
        }
    }
}
