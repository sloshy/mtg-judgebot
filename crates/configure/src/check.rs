//! Whether a draft would start: the draft `judge.toml` and `.env` run
//! through the binaries' own loaders, one surface at a time. Nothing here
//! restates a rule; a rule the loaders gain is checked here with no edit.
//!
//! Two things are left to startup, because they depend on the machine that
//! serves rather than on the files: a cloud endpoint's credential chain
//! (`Config::probe_auth`, which may touch the network) and `--web`'s
//! `WEB_DIST` directory (the image sets its own).

use std::{ffi::OsString, path::Path};

use judge_api::{ApiConfig, Interface, Launch};
use judge_bot::config::{Config, ConfigError, Location};
use nonempty::NonEmpty;
use serde::Serialize;

use crate::env::VARS;

/// A part of the deployment that reads the files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    /// `judge.toml` (or the zero-config setup) and the spend settings:
    /// every binary loads these.
    Models,
    /// The Discord bot (`judge-bot`).
    Bot,
    /// `judge-api`, with the interfaces `API_INTERFACES` opens.
    Api,
    /// The database settings.
    Database,
}

/// One surface's verdict.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum Outcome {
    /// It would start.
    Ok {
        /// What it would run, when there is something to say.
        summary: Option<String>,
    },
    /// It would refuse to start.
    Error {
        /// The loader's message.
        message: String,
        /// The key or variable it is about.
        location: Location,
    },
    /// Not checked, because something it needs failed first.
    Skipped {
        /// Why.
        reason: String,
    },
}

/// A surface and its verdict.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    /// Which.
    pub surface: Surface,
    /// What.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Check a draft: `toml` is the file's text (`None` for no file, the
/// zero-config setup) and `env` the variables as the binaries would see
/// them.
#[must_use]
pub fn run(toml: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Vec<Check> {
    let config = match toml {
        Some(text) => Config::from_toml(text, Path::new("judge.toml"), env),
        None => Config::from_vars(env),
    }
    .and_then(|c| c.models().map(|_| c));
    let models = match &config {
        Ok(c) => Outcome::Ok {
            summary: Some(c.summary()),
        },
        Err(e) => config_error(e),
    };
    // Each binary reads its own settings before the models (bot: Discord's;
    // api: its own and the interfaces'), so their errors stand on their own.
    let config = config.ok();
    vec![
        Check {
            surface: Surface::Database,
            outcome: database(env),
        },
        Check {
            surface: Surface::Models,
            outcome: models,
        },
        Check {
            surface: Surface::Bot,
            outcome: bot(config.as_ref(), env),
        },
        Check {
            surface: Surface::Api,
            outcome: api(config.as_ref(), env),
        },
    ]
}

/// What a surface says once its own settings passed, given the models.
fn with_models(
    config: Option<&Config>,
    operator: impl FnOnce(&Config) -> Result<(), ConfigError>,
    summary: Option<String>,
) -> Outcome {
    match config {
        None => Outcome::Skipped {
            reason: "its own settings pass; the model configuration does not load".to_owned(),
        },
        Some(c) => match operator(c) {
            Ok(()) => Outcome::Ok { summary },
            Err(e) => config_error(&e),
        },
    }
}

fn config_error(e: &ConfigError) -> Outcome {
    Outcome::Error {
        message: e.to_string(),
        location: e.location(),
    }
}

/// An error from a loader that reports through `anyhow`: located at the
/// first registry variable its message names.
fn anyhow_error(e: &anyhow::Error) -> Outcome {
    let message = format!("{e:#}");
    let location = VARS
        .iter()
        .filter_map(|v| names(&message, v.name).map(|at| (at, v.name)))
        .min()
        .map_or(Location::Elsewhere, |(_, var)| Location::Env {
            var: var.to_owned(),
        });
    Outcome::Error { message, location }
}

/// Where `message` names `var` as a whole word.
fn names(message: &str, var: &str) -> Option<usize> {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    message.match_indices(var).map(|(i, _)| i).find(|&i| {
        let before = i.checked_sub(1).and_then(|j| message.as_bytes().get(j));
        let after = message.as_bytes().get(i + var.len());
        !before.is_some_and(|b| word(*b)) && !after.is_some_and(|b| word(*b))
    })
}

fn database(env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let set = |k: &str| {
        env(k)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(port) = set("DB_PORT")
        && !port.parse::<u16>().is_ok_and(|p| p > 0)
    {
        return Outcome::Error {
            message: format!("DB_PORT must be a port number 1..=65535, got {port:?}"),
            location: Location::Env {
                var: "DB_PORT".to_owned(),
            },
        };
    }
    match judge_bot::db::migrate::parse_flag(
        env(judge_bot::db::migrate::AUTO_MIGRATE_ENV).as_deref(),
    ) {
        Ok(_) => Outcome::Ok { summary: None },
        Err(e) => Outcome::Error {
            message: e.to_string(),
            location: Location::Env {
                var: judge_bot::db::migrate::AUTO_MIGRATE_ENV.to_owned(),
            },
        },
    }
}

fn bot(config: Option<&Config>, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    if let Err(e) = judge_bot::discord::Config::from_vars(env) {
        return anyhow_error(&e);
    }
    with_models(config, |c| c.discord_operator().map(|_| ()), None)
}

fn api(config: Option<&Config>, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let flags = env("API_INTERFACES")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "--api --web".to_owned());
    let interfaces =
        match judge_api::interfaces::parse(flags.split_whitespace().map(OsString::from)) {
            Ok(Launch::Serve(i)) => i,
            Ok(Launch::Help) => {
                return Outcome::Error {
                    message: "API_INTERFACES asks for --help, which serves nothing".to_owned(),
                    location: Location::Env {
                        var: "API_INTERFACES".to_owned(),
                    },
                };
            }
            Err(e) => {
                return Outcome::Error {
                    message: format!("API_INTERFACES: {e:#}"),
                    location: Location::Env {
                        var: "API_INTERFACES".to_owned(),
                    },
                };
            }
        };
    let api = match ApiConfig::from_vars(env) {
        Ok(a) => a,
        Err(e) => return anyhow_error(&e),
    };
    // `--web`'s directory is the serving machine's business (the image sets
    // WEB_DIST itself), so check every other interface.
    let others: Vec<Interface> = [Interface::Api, Interface::Mcp]
        .into_iter()
        .filter(|i| interfaces.has(*i))
        .collect();
    if let Some(others) = NonEmpty::from_vec(others)
        && let Err(e) = api.check(&judge_api::Interfaces::of(&others))
    {
        return anyhow_error(&e);
    }
    with_models(
        config,
        |c| c.network_operator().map(|_| ()),
        Some(format!("serving {flags}")),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn checks(toml: Option<&str>, vars: &[(&str, &str)]) -> HashMap<Surface, Outcome> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        run(toml, &|k| vars.get(k).cloned())
            .into_iter()
            .map(|c| (c.surface, c.outcome))
            .collect()
    }

    fn error_at(o: Option<&Outcome>) -> Option<Location> {
        match o {
            Some(Outcome::Error { location, .. }) => Some(location.clone()),
            _ => None,
        }
    }

    fn env(var: &str) -> Location {
        let var = var.to_owned();
        Location::Env { var }
    }

    const OK: &[(&str, &str)] = &[
        ("DATABASE_URL", "postgres://localhost/j"),
        ("ANTHROPIC_API_KEY", "k"),
        ("DISCORD_TOKEN", "t"),
        ("JUDGE_OPERATOR_DISCORD", "somejudge"),
        ("JUDGE_OPERATOR_EMAIL", "judge@example.org"),
    ];

    #[test]
    fn a_complete_zero_config_setup_passes_everywhere() {
        let c = checks(None, OK);
        for (s, o) in &c {
            assert!(matches!(o, Outcome::Ok { .. }), "{s:?}: {o:?}");
        }
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn each_surface_points_at_its_own_variable() {
        let without = |k: &str| -> Vec<(&str, &str)> {
            OK.iter().copied().filter(|(n, _)| *n != k).collect()
        };
        let c = checks(None, &without("JUDGE_OPERATOR_EMAIL"));
        assert_eq!(
            error_at(c.get(&Surface::Api)),
            Some(env("JUDGE_OPERATOR_EMAIL"))
        );
        assert!(matches!(c.get(&Surface::Bot), Some(Outcome::Ok { .. })));
        let c = checks(None, &without("DISCORD_TOKEN"));
        assert_eq!(error_at(c.get(&Surface::Bot)), Some(env("DISCORD_TOKEN")));
        let mut bad = OK.to_vec();
        bad.push(("JUDGE_USER_WINDOW_SECS", "0"));
        bad.push(("API_INTERFACES", "--api --mcp"));
        let c = checks(None, &bad);
        assert_eq!(
            error_at(c.get(&Surface::Bot)),
            Some(env("JUDGE_USER_WINDOW_SECS"))
        );
        assert_eq!(error_at(c.get(&Surface::Api)), Some(env("MCP_TOKEN")));
        let mut bad = OK.to_vec();
        bad.push(("JUDGE_REFRESH_HOURS", "daily"));
        let c = checks(None, &bad);
        assert_eq!(
            error_at(c.get(&Surface::Models)),
            Some(env("JUDGE_REFRESH_HOURS"))
        );
        let c = checks(None, &without("ANTHROPIC_API_KEY"));
        assert_eq!(
            error_at(c.get(&Surface::Models)),
            Some(env("ANTHROPIC_API_KEY"))
        );
        assert!(matches!(
            c.get(&Surface::Bot),
            Some(Outcome::Skipped { .. })
        ));
    }

    #[test]
    fn a_file_is_checked_by_the_loader() {
        let toml = "[providers.p]\nkind = \"anthropic\"\nendpoint = \"proxy\"\napi_key_env = \"K\"\n\
                    [models.extract]\nprovider = \"p\"\nmodel = \"m\"\n\
                    [models.synth]\nprovider = \"p\"\nmodel = \"m\"\n";
        let c = checks(Some(toml), &[("K", "k")]);
        assert_eq!(
            error_at(c.get(&Surface::Models)),
            Some(Location::Toml {
                path: "providers.p.base_url".to_owned()
            })
        );
    }

    #[test]
    fn names_matches_whole_words() {
        assert_eq!(
            names(
                "JUDGE_USER_LIMIT_X and JUDGE_USER_LIMIT",
                "JUDGE_USER_LIMIT"
            ),
            Some(23)
        );
        assert_eq!(names("XJUDGE_ROLE", "JUDGE_ROLE"), None);
    }
}
