//! Whether a draft would start: the draft `judge.toml` and `.env` run
//! through the binaries' own loaders, one surface at a time. Nothing here
//! restates a rule; a rule the loaders gain is checked here with no edit.
//!
//! The roles are the ones the compose `judgebot` service would run
//! ([`compose_roles`]: `JUDGE_ROLES`, else every role but `--mcp` with the
//! deprecated `API_INTERFACES`), and a role it would not run is not checked:
//! a deployment without Discord needs no `DISCORD_TOKEN`.
//!
//! Two things are left to startup, because they depend on the machine that
//! serves rather than on the files: a cloud endpoint's credential chain
//! (`Config::probe_auth`, which may touch the network) and `--web`'s
//! `WEB_DIST` directory (the image sets its own).

use std::path::Path;

use judge_api::{ApiConfig, Interface};
use judge_bot::config::{Config, ConfigError, Location};
use judgebot::roles::{API_INTERFACES_ENV, ROLES_ENV, Role, Roles, compose_roles};
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
    /// The roles the compose `judgebot` service runs.
    Roles,
    /// The `--discord` role.
    Discord,
    /// The network roles (`--api`, `--web`, `--mcp`).
    Http,
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
    let (roles, roles_outcome) = match compose_roles(
        env(ROLES_ENV).as_deref(),
        env(API_INTERFACES_ENV).as_deref(),
    ) {
        Ok((r, origin)) => {
            let summary = format!("{} (from {origin})", r.flags());
            (
                Some(r),
                Outcome::Ok {
                    summary: Some(summary),
                },
            )
        }
        Err(e) => (None, anyhow_error(&e)),
    };
    // Each role reads its own settings before the models (--discord:
    // Discord's; the network roles: the API's and the interfaces'), so their
    // errors stand on their own.
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
            surface: Surface::Roles,
            outcome: roles_outcome,
        },
        Check {
            surface: Surface::Discord,
            outcome: discord(roles.as_ref(), config.as_ref(), env),
        },
        Check {
            surface: Surface::Http,
            outcome: http(roles.as_ref(), config.as_ref(), env),
        },
    ]
}

/// Why a role's surface is not checked: the roles do not parse, or they
/// leave it out (`what` says which, ending in "is" or "is not").
fn not_run(roles: Option<&Roles>, what: &str) -> Outcome {
    Outcome::Skipped {
        reason: match roles {
            None => "the roles do not parse".to_owned(),
            Some(r) => format!("{what} among the roles ({})", r.flags()),
        },
    }
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

fn discord(
    roles: Option<&Roles>,
    config: Option<&Config>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Outcome {
    if !roles.is_some_and(|r| r.iter().any(|r| r == Role::Discord)) {
        return not_run(roles, "--discord is not");
    }
    if let Err(e) = judge_bot::discord::Config::from_vars(env) {
        return anyhow_error(&e);
    }
    with_models(config, |c| c.discord_operator().map(|_| ()), None)
}

fn http(
    roles: Option<&Roles>,
    config: Option<&Config>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Outcome {
    let interfaces: Vec<Interface> = roles
        .into_iter()
        .flat_map(Roles::iter)
        .filter_map(Role::interface)
        .collect();
    let Some(interfaces) = NonEmpty::from_vec(interfaces) else {
        return not_run(roles, "none of --api, --web and --mcp is");
    };
    let interfaces = judge_api::Interfaces::of(&interfaces);
    let flags = interfaces
        .iter()
        .map(Interface::flag)
        .collect::<Vec<_>>()
        .join(" ");
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
        return anyhow_error(&anyhow::Error::from(e));
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
        assert_eq!(c.len(), 5);
    }

    fn skipped(o: Option<&Outcome>) -> bool {
        matches!(o, Some(Outcome::Skipped { .. }))
    }

    /// A role the compose service would not run is not checked, so a
    /// deployment without Discord needs no Discord settings.
    #[test]
    fn only_the_roles_that_run_are_checked() {
        let without_discord: Vec<(&str, &str)> = OK
            .iter()
            .copied()
            .filter(|(n, _)| !n.contains("DISCORD"))
            .chain([("JUDGE_ROLES", "--api --web --jobs")])
            .collect();
        let c = checks(None, &without_discord);
        assert!(skipped(c.get(&Surface::Discord)), "{c:?}");
        assert!(
            matches!(c.get(&Surface::Http), Some(Outcome::Ok { .. })),
            "{c:?}"
        );
        assert!(
            matches!(c.get(&Surface::Roles), Some(Outcome::Ok { summary: Some(s) })
                if s == "--api --web --jobs (from JUDGE_ROLES)"),
            "{c:?}"
        );

        let mut bot_only = OK.to_vec();
        bot_only.retain(|(n, _)| *n != "JUDGE_OPERATOR_EMAIL");
        bot_only.push(("JUDGE_ROLES", "--discord --jobs"));
        let c = checks(None, &bot_only);
        assert!(skipped(c.get(&Surface::Http)), "{c:?}");
        assert!(
            matches!(c.get(&Surface::Discord), Some(Outcome::Ok { .. })),
            "{c:?}"
        );

        // Neither variable: every role but --mcp, as before roles existed.
        let c = checks(None, OK);
        assert!(
            matches!(c.get(&Surface::Roles), Some(Outcome::Ok { summary: Some(s) })
                if s.starts_with("--discord --api --web --jobs")),
            "{c:?}"
        );
    }

    #[test]
    fn a_bad_role_list_points_at_its_variable() {
        for (var, value) in [
            ("JUDGE_ROLES", "--discord --bot"),
            ("API_INTERFACES", "--discord"),
            ("API_INTERFACES", "--help"),
        ] {
            let mut bad = OK.to_vec();
            bad.push((var, value));
            let c = checks(None, &bad);
            assert_eq!(error_at(c.get(&Surface::Roles)), Some(env(var)), "{value}");
            assert!(skipped(c.get(&Surface::Discord)) && skipped(c.get(&Surface::Http)));
        }
    }

    #[test]
    fn each_surface_points_at_its_own_variable() {
        let without = |k: &str| -> Vec<(&str, &str)> {
            OK.iter().copied().filter(|(n, _)| *n != k).collect()
        };
        let c = checks(None, &without("JUDGE_OPERATOR_EMAIL"));
        assert_eq!(
            error_at(c.get(&Surface::Http)),
            Some(env("JUDGE_OPERATOR_EMAIL"))
        );
        assert!(matches!(c.get(&Surface::Discord), Some(Outcome::Ok { .. })));
        let c = checks(None, &without("DISCORD_TOKEN"));
        assert_eq!(
            error_at(c.get(&Surface::Discord)),
            Some(env("DISCORD_TOKEN"))
        );
        let mut bad = OK.to_vec();
        bad.push(("JUDGE_USER_WINDOW_SECS", "0"));
        bad.push(("API_INTERFACES", "--api --mcp"));
        let c = checks(None, &bad);
        assert_eq!(
            error_at(c.get(&Surface::Discord)),
            Some(env("JUDGE_USER_WINDOW_SECS"))
        );
        assert_eq!(error_at(c.get(&Surface::Http)), Some(env("MCP_TOKEN")));
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
        assert!(skipped(c.get(&Surface::Discord)));
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
