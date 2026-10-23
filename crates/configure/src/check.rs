//! Whether a draft would start: the draft `judge.toml` and `.env` run
//! through the binaries' own loaders, one surface at a time. Nothing here
//! restates a rule; a rule the loaders gain is checked here with no edit.
//!
//! The roles are the ones the compose `judgebot` service would run
//! ([`compose_roles`]: `JUDGE_ROLES`, else every role but `--mcp` and
//! `--admin`; a set `API_INTERFACES`, retired, fails), and a role it would
//! not run is not checked:
//! a deployment without Discord needs no `DISCORD_TOKEN`.
//!
//! Two things are left to startup, because they depend on the machine that
//! serves rather than on the files: a cloud endpoint's credential chain
//! (`Config::probe_auth`, which may touch the network) and the `WEB_DIST`
//! directory `--web` and `--admin` serve (the image sets its own).

use std::path::Path;

use judge_api::{ApiConfig, Interface, admin::AdminConfig};
use judge_bot::admin::{ADMIN_ADDR_ENV, ADMIN_PORT_ENV, COMPOSE_ADDR};
use judge_bot::config::{Config, ConfigError, Location};
use judge_core::Locale;
use judgebot::roles::{API_INTERFACES_ENV, ROLES_ENV, Role, Roles, compose_roles};
use nonempty::NonEmpty;
use serde::Serialize;

use crate::{
    catalog::{self, Absent, Messages},
    env::VARS,
};

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
    /// The `--admin` role.
    Admin,
    /// The database settings.
    Database,
    /// Where compose opens the instance: `127.0.0.1:8787` always, the
    /// `public` profile's HTTPS site (`JUDGE_DOMAIN`) and the `tunnel`
    /// profile's cloudflared.
    Exposure,
}

/// The public host name the compose `edge` service (the `public` profile)
/// serves and gets a certificate for. Read by Caddy alone, so checked here.
pub const DOMAIN_ENV: &str = "JUDGE_DOMAIN";

/// The compose profiles `scripts/deploy.sh` starts.
pub const PROFILES_ENV: &str = "COMPOSE_PROFILES";

/// The profiles of `COMPOSE_PROFILES` that open the instance beyond this
/// host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Exposure {
    /// `public`: the `edge` service on ports 80 and 443.
    pub public: bool,
    /// `tunnel`: cloudflared.
    pub tunnel: bool,
}

impl Exposure {
    /// Read from `COMPOSE_PROFILES` (comma-separated, as compose reads it).
    #[must_use]
    pub fn of(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let profiles = env(PROFILES_ENV).unwrap_or_default();
        let has = |p: &str| profiles.split(',').any(|q| q.trim() == p);
        Self {
            public: has("public"),
            tunnel: has("tunnel"),
        }
    }
}

/// Whether `s` is a host name Caddy can get a certificate for: dot-separated
/// labels of letters, digits and inner hyphens, at least two of them, the
/// last not all digits (an IP address is not a domain), with no scheme, port,
/// path or trailing dot.
#[must_use]
pub fn is_host_name(s: &str) -> bool {
    let label = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let labels: Vec<&str> = s.split('.').collect();
    s.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| label(l))
        && labels
            .last()
            .is_some_and(|tld| !tld.bytes().all(|b| b.is_ascii_digit()))
}

/// A variable's value, trimmed, when it is set and not blank.
fn set_var(env: &dyn Fn(&str) -> Option<String>, k: &str) -> Option<String> {
    env(k)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
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
/// them. The checks' own lines are in `locale`; the loaders' refusals are
/// quoted as they come, in English (`catalog`).
#[must_use]
pub fn run(locale: Locale, toml: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Vec<Check> {
    let m = catalog::messages(locale);
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
            let summary = (m.roles_from)(&r.flags(), origin);
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
            outcome: database(m, env),
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
            outcome: discord(m, roles.as_ref(), config.as_ref(), env),
        },
        Check {
            surface: Surface::Http,
            outcome: http(m, roles.as_ref(), config.as_ref(), env),
        },
        Check {
            surface: Surface::Admin,
            outcome: admin(m, roles.as_ref(), env),
        },
        Check {
            surface: Surface::Exposure,
            outcome: exposure(m, env),
        },
    ]
}

/// The `public` profile's site needs its host name: without one the `edge`
/// container refuses to start, and with a malformed one Caddy cannot get a
/// certificate. The summary lists every route in.
fn exposure(m: &Messages, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let open = Exposure::of(env);
    let domain = set_var(env, DOMAIN_ENV);
    let location = || Location::Env {
        var: DOMAIN_ENV.to_owned(),
    };
    if open.public {
        match domain.as_deref() {
            None => {
                return Outcome::Error {
                    message: m.domain_missing.to_owned(),
                    location: location(),
                };
            }
            Some(d) if !is_host_name(d) => {
                return Outcome::Error {
                    message: (m.bad_domain)(d),
                    location: location(),
                };
            }
            Some(_) => {}
        }
    }
    let public = domain.as_deref().filter(|_| open.public);
    Outcome::Ok {
        summary: Some((m.exposure)(public, open.tunnel)),
    }
}

/// Warnings about exposure that no loader sees: the `public` profile's or
/// the tunnel's visitors in one rate-limit bucket, and a host name nothing
/// serves.
#[must_use]
pub fn exposure_warnings(m: &Messages, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let open = Exposure::of(env);
    let domain = set_var(env, DOMAIN_ENV);
    let mut out = vec![];
    let mode =
        judge_api::config::ClientIpSource::parse(env(judge_api::config::CLIENT_IP_ENV).as_deref())
            .ok();
    if open.public
        && let Some(d) = &domain
        && let Some(mode) = mode
        && mode != judge_api::config::ClientIpSource::ProxyHeader
    {
        out.push((m.public_client_ip)(d, mode.name()));
    }
    if open.tunnel && mode == Some(judge_api::config::ClientIpSource::ProxyHeader) {
        out.push(m.tunnel_client_ip.to_owned());
    }
    if !open.public && domain.is_some() {
        out.push(m.domain_unused.to_owned());
    }
    out
}

/// `--admin`: its token (well formed, its own) and the host port compose
/// publishes it on. The address is the one the compose service sets
/// (`COMPOSE_ADDR`), whatever `.env` says, since that is what the container
/// listens on. The page it serves is the serving machine's business, as
/// `--web`'s is. `ADMIN_PORT` is checked whatever the roles: the proxy
/// publishes it either way, and a bad one stops `docker compose up`.
fn admin(m: &Messages, roles: Option<&Roles>, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    let as_composed = |k: &str| {
        if k == ADMIN_ADDR_ENV {
            Some(COMPOSE_ADDR.to_owned())
        } else {
            env(k)
        }
    };
    let cfg = AdminConfig::from_vars(as_composed);
    if !roles.is_some_and(|r| r.iter().any(|r| r == Role::Admin)) {
        return match cfg {
            Err(e) if format!("{e:#}").contains(ADMIN_PORT_ENV) => anyhow_error(&e),
            _ => not_run(m, roles, Absent::Admin),
        };
    }
    match cfg {
        Err(e) => anyhow_error(&e),
        Ok(cfg) if cfg.settings.token.is_none() => {
            anyhow_error(&anyhow::anyhow!("{}", judge_api::admin::AdminUnmet::Token))
        }
        Ok(cfg) => Outcome::Ok {
            summary: Some((m.admin_listens)(
                COMPOSE_ADDR,
                &cfg.settings.browser_addr().to_string(),
            )),
        },
    }
}

/// Why a role's surface is not checked: the roles do not parse, or they
/// leave it out (`absent` says which).
fn not_run(m: &Messages, roles: Option<&Roles>, absent: Absent) -> Outcome {
    Outcome::Skipped {
        reason: match roles {
            None => m.roles_unparsed.to_owned(),
            Some(r) => (m.not_among_roles)(absent, &r.flags()),
        },
    }
}

/// What a surface says once its own settings passed, given the models.
fn with_models(
    m: &Messages,
    config: Option<&Config>,
    operator: impl FnOnce(&Config) -> Result<(), ConfigError>,
    summary: Option<String>,
) -> Outcome {
    match config {
        None => Outcome::Skipped {
            reason: m.models_fail.to_owned(),
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

fn database(m: &Messages, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
    if let Some(port) = set_var(env, "DB_PORT")
        && !port.parse::<u16>().is_ok_and(|p| p > 0)
    {
        return Outcome::Error {
            message: (m.bad_db_port)(&port),
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
    m: &Messages,
    roles: Option<&Roles>,
    config: Option<&Config>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Outcome {
    if !roles.is_some_and(|r| r.iter().any(|r| r == Role::Discord)) {
        return not_run(m, roles, Absent::Discord);
    }
    let cfg = match judge_bot::discord::Config::from_vars(env) {
        Ok(cfg) => cfg,
        Err(e) => return anyhow_error(&e),
    };
    // Over HTTP with no network role, the interactions endpoint binds
    // API_ADDR itself, as `roles::plan` checks.
    let listens = roles.is_some_and(|r| r.iter().any(|r| r.interface().is_some()));
    if cfg.interactions == judge_bot::discord::Delivery::Http
        && !listens
        && let Err(e) = ApiConfig::from_vars(env)
    {
        return anyhow_error(&e);
    }
    with_models(m, config, |c| c.discord_operator().map(|_| ()), None)
}

fn http(
    m: &Messages,
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
        return not_run(m, roles, Absent::Network);
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
        m,
        config,
        |c| c.network_operator().map(|_| ()),
        Some((m.serving)(&flags)),
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
        run(Locale::En, toml, &|k| vars.get(k).cloned())
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
            if *s == Surface::Admin {
                assert!(skipped(Some(o)), "--admin is opt-in: {o:?}");
            } else {
                assert!(matches!(o, Outcome::Ok { .. }), "{s:?}: {o:?}");
            }
        }
        assert_eq!(c.len(), 7);
    }

    fn summary(o: Option<&Outcome>) -> Option<String> {
        match o {
            Some(Outcome::Ok { summary }) => summary.clone(),
            _ => None,
        }
    }

    /// The `public` profile needs a host name it can get a certificate for;
    /// without the profile nothing reads one.
    #[test]
    fn the_public_profile_needs_a_host_name() {
        let mut with = OK.to_vec();
        let c = checks(None, &with);
        assert_eq!(
            summary(c.get(&Surface::Exposure)),
            Some("127.0.0.1:8787 on this host".to_owned())
        );
        with.push(("COMPOSE_PROFILES", "backup, public"));
        let c = checks(None, &with);
        assert_eq!(
            error_at(c.get(&Surface::Exposure)),
            Some(env("JUDGE_DOMAIN"))
        );
        for bad in [
            "https://judge.example.com",
            "judge.example.com:443",
            "judge.example.com/",
            "localhost",
            "203.0.113.9",
            "judge.example.com.",
            "-judge.example.com",
            "judge_bot.example.com",
            "judge..example.com",
        ] {
            let mut bad_domain = with.clone();
            bad_domain.push(("JUDGE_DOMAIN", bad));
            let c = checks(None, &bad_domain);
            assert_eq!(
                error_at(c.get(&Surface::Exposure)),
                Some(env("JUDGE_DOMAIN")),
                "{bad}"
            );
        }
        with.push(("JUDGE_DOMAIN", " judge.example.com "));
        let c = checks(None, &with);
        assert_eq!(
            summary(c.get(&Surface::Exposure)),
            Some(
                "127.0.0.1:8787 on this host; https://judge.example.com on ports 80 and 443"
                    .to_owned()
            )
        );
        with.retain(|(k, _)| *k != "COMPOSE_PROFILES");
        with.push(("COMPOSE_PROFILES", "tunnel,public"));
        let c = checks(None, &with);
        assert!(
            summary(c.get(&Surface::Exposure))
                .is_some_and(|s| s.ends_with("; the Cloudflare tunnel")),
            "{c:?}"
        );
        assert!(is_host_name("xn--bcher-kva.example"));
        assert!(is_host_name("a.b.c.example.org"));
    }

    /// The `public` profile with another client-address mode puts every
    /// visitor in one bucket, and a host name without the profile serves
    /// nothing: both are said, neither refused.
    #[test]
    fn the_public_profile_suggests_the_proxy_client_address() {
        let warn = |vars: &[(&str, &str)]| {
            let vars: HashMap<String, String> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            exposure_warnings(catalog::messages(Locale::En), &|k| vars.get(k).cloned())
        };
        let public = [
            ("COMPOSE_PROFILES", "public"),
            ("JUDGE_DOMAIN", "judge.example.com"),
        ];
        let w = warn(&public);
        assert!(
            matches!(w.as_slice(), [one] if one.contains("API_CLIENT_IP=proxy") && one.contains("peer")),
            "{w:?}"
        );
        let mut cf = public.to_vec();
        cf.push(("API_CLIENT_IP", "cloudflare"));
        assert!(warn(&cf).iter().any(|w| w.contains("cloudflare")));
        let mut proxy = public.to_vec();
        proxy.push(("API_CLIENT_IP", "proxy"));
        assert!(warn(&proxy).is_empty());
        let w = warn(&[("JUDGE_DOMAIN", "judge.example.com")]);
        assert!(
            matches!(w.as_slice(), [one] if one.contains("COMPOSE_PROFILES")),
            "{w:?}"
        );
        assert!(warn(&[]).is_empty());
        // The tunnel's path removes the proxy header, so proxy mode puts its
        // visitors in one bucket; cloudflare is its mode.
        let w = warn(&[("COMPOSE_PROFILES", "tunnel"), ("API_CLIENT_IP", "proxy")]);
        assert!(
            matches!(w.as_slice(), [one] if one.contains("API_CLIENT_IP=cloudflare")),
            "{w:?}"
        );
        assert!(
            warn(&[
                ("COMPOSE_PROFILES", "tunnel"),
                ("API_CLIENT_IP", "cloudflare")
            ])
            .is_empty()
        );
    }

    /// The `edge` container's own guard (`proxy/edge-entrypoint.sh`)
    /// accepts exactly the host names this check does, so a draft the editor
    /// passes starts and one it refuses does not.
    #[test]
    fn the_edge_guard_agrees_with_the_check() -> Result<(), std::io::Error> {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proxy/edge-entrypoint.sh");
        let long_label = format!("{}.example", "a".repeat(64));
        let long_name = format!("{}example", "abcdefghi.".repeat(25));
        let cases = [
            "judge.example.com",
            "a.b",
            "xn--bcher-kva.example",
            "judge-1.example.org",
            "",
            "localhost",
            "203.0.113.9",
            "-",
            "https://judge.example.com",
            "judge.example.com:443",
            "judge.example.com/x",
            "judge.example.com.",
            ".judge.example.com",
            "judge..example.com",
            "-judge.example.com",
            "judge-.example.com",
            "judge.-example.com",
            "judge_bot.example.com",
            "judge example.com",
            "example.123",
            &long_label,
            &long_name,
        ];
        for d in cases {
            let status = std::process::Command::new("sh")
                .arg(&script)
                .arg("true")
                .env("JUDGE_DOMAIN", d)
                .stderr(std::process::Stdio::null())
                .status()?;
            assert_eq!(status.success(), is_host_name(d), "{d:?}");
        }
        Ok(())
    }

    /// The `edge` service reads the variable the check names, under the
    /// profile it names.
    #[test]
    fn the_compose_file_serves_the_domain_under_the_public_profile() {
        let compose = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker-compose.yml"),
        )
        .unwrap_or_default();
        assert!(
            compose.contains("profiles: [\"public\"]"),
            "no service under the public profile"
        );
        assert!(
            compose.contains(&format!("{DOMAIN_ENV}: ${{{DOMAIN_ENV}:-}}")),
            "the edge service no longer reads {DOMAIN_ENV}"
        );
        assert!(
            compose.contains("entrypoint: [\"sh\", \"/etc/caddy/edge-entrypoint.sh\"]"),
            "the edge service no longer runs its guard"
        );
    }

    /// `--admin` is checked when the roles run it: its own token, distinct
    /// from the MCP ones.
    #[test]
    fn the_admin_role_needs_its_own_token() {
        let mut with = OK.to_vec();
        with.push(("JUDGE_ROLES", "--discord --api --web --admin --jobs"));
        let c = checks(None, &with);
        assert_eq!(error_at(c.get(&Surface::Admin)), Some(env("ADMIN_TOKEN")));
        let token = "0123456789abcdef0123456789abcdef";
        with.push(("ADMIN_TOKEN", token));
        let c = checks(None, &with);
        assert!(
            matches!(c.get(&Surface::Admin), Some(Outcome::Ok { summary: Some(s) })
                if s.ends_with("publishes it on 127.0.0.1:8789")),
            "{c:?}"
        );
        // The address compose sets wins over one in .env; ADMIN_PORT moves
        // the published port.
        with.push(("ADMIN_ADDR", "10.0.0.1:1234"));
        with.push(("ADMIN_PORT", "9789"));
        let c = checks(None, &with);
        assert!(
            matches!(c.get(&Surface::Admin), Some(Outcome::Ok { summary: Some(s) })
                if s.contains("0.0.0.0:8789") && s.ends_with("127.0.0.1:9789")),
            "{c:?}"
        );
        with.retain(|(k, _)| *k != "ADMIN_PORT" && *k != "JUDGE_ROLES");
        with.push(("ADMIN_PORT", "0"));
        let c = checks(None, &with);
        assert_eq!(
            error_at(c.get(&Surface::Admin)),
            Some(env("ADMIN_PORT")),
            "checked without --admin too"
        );
        with.retain(|(k, _)| *k != "ADMIN_PORT");
        with.push(("JUDGE_ROLES", "--discord --api --web --admin --jobs"));
        with.push(("MCP_TOKEN", token));
        let c = checks(None, &with);
        assert_eq!(error_at(c.get(&Surface::Admin)), Some(env("ADMIN_TOKEN")));
        assert_eq!(error_at(c.get(&Surface::Http)), Some(env("MCP_TOKEN")));
    }

    /// The compose file sets `ADMIN_ADDR` to what the check assumes, and
    /// publishes `ADMIN_PORT` on loopback only.
    #[test]
    fn the_compose_file_sets_the_admin_address_the_check_assumes() {
        let compose = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker-compose.yml"),
        )
        .unwrap_or_default();
        assert!(
            compose
                .lines()
                .any(|l| l.trim() == format!("ADMIN_ADDR: {COMPOSE_ADDR}")),
            "docker-compose.yml no longer sets ADMIN_ADDR: {COMPOSE_ADDR}"
        );
        assert!(
            compose.contains("- \"127.0.0.1:${ADMIN_PORT:-8789}:8789\""),
            "the proxy's admin port mapping changed"
        );
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

    /// `--discord` alone over HTTP binds `API_ADDR` itself, so a bad one is
    /// a Discord error even though no network role runs.
    #[test]
    fn discord_over_http_alone_checks_its_listen_address() {
        let mut alone = OK.to_vec();
        alone.extend([
            ("JUDGE_ROLES", "--discord"),
            ("DISCORD_INTERACTIONS", "http"),
            ("API_ADDR", "nowhere"),
        ]);
        let c = checks(None, &alone);
        assert!(
            matches!(c.get(&Surface::Discord), Some(Outcome::Error { .. })),
            "{c:?}"
        );
        alone.retain(|(n, _)| *n != "API_ADDR");
        let c = checks(None, &alone);
        assert!(
            matches!(c.get(&Surface::Discord), Some(Outcome::Ok { .. })),
            "{c:?}"
        );
    }

    #[test]
    fn a_bad_role_list_points_at_its_variable() {
        for (var, value) in [
            ("JUDGE_ROLES", "--discord --bot"),
            // Retired: the fix is the JUDGE_ROLES line the error names.
            ("API_INTERFACES", "--api --mcp"),
        ] {
            let mut bad = OK.to_vec();
            bad.push((var, value));
            let c = checks(None, &bad);
            assert_eq!(
                error_at(c.get(&Surface::Roles)),
                Some(env("JUDGE_ROLES")),
                "{var}={value}"
            );
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
        bad.push(("JUDGE_ROLES", "--discord --api --mcp --jobs"));
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

    /// The checks' own lines are in the page's language; a loader's refusal
    /// is quoted as it comes.
    #[test]
    fn the_checks_own_lines_are_in_the_requested_locale() {
        let mut vars: HashMap<String, String> = OK
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        vars.insert("JUDGE_ROLES".to_owned(), "--discord --jobs".to_owned());
        vars.insert("DB_PORT".to_owned(), "x".to_owned());
        vars.remove("DISCORD_TOKEN");
        let c: HashMap<Surface, Outcome> = run(Locale::Stub, None, &|k| vars.get(k).cloned())
            .into_iter()
            .map(|c| (c.surface, c.outcome))
            .collect();
        assert!(
            matches!(c.get(&Surface::Roles), Some(Outcome::Ok { summary: Some(s) })
                if s == "⟦roles_from --discord --jobs Roles⟧"),
            "{c:?}"
        );
        assert!(
            matches!(c.get(&Surface::Http), Some(Outcome::Skipped { reason })
                if reason == "⟦not_among_roles Network --discord --jobs⟧"),
            "{c:?}"
        );
        assert!(
            matches!(c.get(&Surface::Database), Some(Outcome::Error { message, .. })
                if message == "⟦bad_db_port x⟧"),
            "{c:?}"
        );
        assert!(
            matches!(c.get(&Surface::Discord), Some(Outcome::Error { message, .. })
                if message.contains("DISCORD_TOKEN") && !message.contains('⟦')),
            "{c:?}"
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
