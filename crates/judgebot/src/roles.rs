//! What a `judgebot` process does, and what each of those things requires.
//!
//! A process runs a set of [`Role`]s chosen at launch. The set is a
//! [`NonEmpty`], so "a process that does nothing" is not a value this program
//! can hold. [`plan`] turns the set into a [`Plan`] by checking each role's
//! requirements against the environment before anything connects or binds.
//! It matches every role exhaustively, and the plan's parts are the types the
//! roles run on (`discord::Config` holds the token, [`DiscordOperator`] and
//! [`Network`] can only be made by passing their checks), so a role added
//! later cannot start without saying what it needs.

use std::fmt;

use anyhow::Result;
use judge_api::{ApiConfig, Interface, Interfaces, Network, Refused};
use judge_bot::{
    Models,
    config::Config as JudgeConfig,
    discord,
    jobs::{Jobs, Schedule},
};
use judge_core::DiscordOperator;
use nonempty::NonEmpty;

/// The variable holding the roles when the command line names none.
pub const ROLES_ENV: &str = "JUDGE_ROLES";

/// One thing a `judgebot` process can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The Discord bot: the gateway connection and the slash commands.
    Discord,
    /// `POST /api/judge`, the anonymous question route.
    Api,
    /// The built web page, from `WEB_DIST`.
    Web,
    /// The MCP transport at `/mcp`, behind `MCP_TOKEN`.
    Mcp,
    /// The scheduled data refresh (`JUDGE_REFRESH_HOURS`).
    Jobs,
}

impl Role {
    /// Every role, in the order the usage text and the logs list them.
    pub const ALL: [Self; 5] = [Self::Discord, Self::Api, Self::Web, Self::Mcp, Self::Jobs];

    /// The flag that names it.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Discord => "--discord",
            Self::Api => "--api",
            Self::Web => "--web",
            Self::Mcp => "--mcp",
            Self::Jobs => "--jobs",
        }
    }

    /// Its name in a log line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Api => "api",
            Self::Web => "web",
            Self::Mcp => "mcp",
            Self::Jobs => "jobs",
        }
    }

    /// Does it serve anything (and so call a model)?
    #[must_use]
    pub const fn serves(self) -> bool {
        match self {
            Self::Discord | Self::Api | Self::Web | Self::Mcp => true,
            Self::Jobs => false,
        }
    }

    /// The network interface it is, if it is one.
    #[must_use]
    pub const fn interface(self) -> Option<Interface> {
        match self {
            Self::Api => Some(Interface::Api),
            Self::Web => Some(Interface::Web),
            Self::Mcp => Some(Interface::Mcp),
            Self::Discord | Self::Jobs => None,
        }
    }

    fn from_flag(arg: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.flag() == arg)
    }
}

impl From<Interface> for Role {
    fn from(interface: Interface) -> Self {
        match interface {
            Interface::Api => Self::Api,
            Interface::Web => Self::Web,
            Interface::Mcp => Self::Mcp,
        }
    }
}

/// The roles of one launch: never empty, no repeats, in [`Role::ALL`]'s
/// order, so two ways of naming the same roles are the same value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roles(NonEmpty<Role>);

impl Roles {
    /// The set of `roles`, duplicates collapsed.
    #[must_use]
    pub fn of(roles: &NonEmpty<Role>) -> Self {
        let canonical: Vec<Role> = Role::ALL
            .into_iter()
            .filter(|r| roles.contains(r))
            .collect();
        // Every variant is in `ALL`, so the filter keeps at least the head;
        // the fallback (the input as given) costs only the order.
        Self(NonEmpty::from_vec(canonical).unwrap_or_else(|| roles.clone()))
    }

    /// The network interfaces and `--jobs`: what `judge-api` always was.
    #[must_use]
    pub fn api_compat(interfaces: &Interfaces) -> Self {
        let mut roles = NonEmpty::new(Role::Jobs);
        roles.extend(interfaces.iter().map(Role::from));
        Self::of(&roles)
    }

    /// The roles, in order.
    pub fn iter(&self) -> impl Iterator<Item = Role> + '_ {
        self.0.iter().copied()
    }

    /// The roles as flags: `--discord --api`, a command line that names them.
    #[must_use]
    pub fn flags(&self) -> String {
        self.iter().map(Role::flag).collect::<Vec<_>>().join(" ")
    }

    /// The roles this launch left off, for the startup log.
    #[must_use]
    pub fn off(&self) -> String {
        names(Role::ALL.into_iter().filter(|r| !self.0.contains(r)))
    }
}

impl fmt::Display for Roles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&names(self.iter()))
    }
}

fn names(roles: impl Iterator<Item = Role>) -> String {
    let names: Vec<&str> = roles.map(Role::name).collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// Parse role flags (the command line, or [`ROLES_ENV`] split on
/// whitespace). `None` when there are none; `from` names the source in an
/// error.
///
/// # Errors
/// An argument that is not a role flag, or a role named twice.
pub fn parse<S: AsRef<str>>(
    args: impl IntoIterator<Item = S>,
    from: &str,
    usage: &str,
) -> Result<Option<Roles>> {
    let mut chosen: Vec<Role> = vec![];
    for arg in args {
        let arg = arg.as_ref();
        let Some(role) = Role::from_flag(arg) else {
            anyhow::bail!("{from}: unknown argument {arg:?}\n\n{usage}");
        };
        // A role named twice is a mistake, and folding it would hide which
        // line the operator meant to write.
        anyhow::ensure!(
            !chosen.contains(&role),
            "{from}: {} given more than once\n\n{usage}",
            role.flag()
        );
        chosen.push(role);
    }
    Ok(NonEmpty::from_vec(chosen).map(|r| Roles::of(&r)))
}

/// The variable that chose `judge-api`'s interfaces in the two-service
/// compose file. Deprecated: `docker-compose.yml` still reads it when
/// [`ROLES_ENV`] is unset ([`COMPOSE_COMMAND`]), and the binary only warns
/// about it ([`api_interfaces_warning`]).
pub const API_INTERFACES_ENV: &str = "API_INTERFACES";

/// The `command:` of `docker-compose.yml`'s `judgebot` service, verbatim
/// (`roles_match_the_compose_file` holds the file to it). Compose
/// interpolates it before the binary starts: [`ROLES_ENV`] when it is set and
/// not empty, else the roles the old `bot` and `api` services ran together,
/// `--discord --jobs` and [`API_INTERFACES_ENV`] (or the `--api --web` the
/// `api` service defaulted to). [`compose_roles`] is the same rule in Rust.
///
/// The template was compiled against compose-go v1.16.0, the interpolation
/// library Docker Compose v2.20.0 pins (Synology's Container Manager ships
/// v2.20), and resolves every case there as [`compose_roles`] does. The v2.20
/// binary itself was not run.
pub const COMPOSE_COMMAND: &str = "${JUDGE_ROLES:---discord --jobs ${API_INTERFACES:---api --web}}";

/// What decided the roles [`compose_roles`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeOrigin {
    /// [`ROLES_ENV`].
    Roles,
    /// [`API_INTERFACES_ENV`], deprecated, after `--discord --jobs`.
    ApiInterfaces,
    /// Neither is set: every role but `--mcp`.
    Default,
}

impl fmt::Display for ComposeOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Roles => ROLES_ENV,
            Self::ApiInterfaces => "API_INTERFACES (deprecated)",
            Self::Default => "the compose file's default",
        })
    }
}

/// The roles the compose `judgebot` service runs, given the two variables'
/// values in `.env`: [`COMPOSE_COMMAND`] as Compose interpolates it (`:-`,
/// so an empty value counts as unset) and the binary then reads it.
///
/// # Errors
/// What the binary would refuse: an argument that is not a role flag, a
/// role named twice (`API_INTERFACES=--discord`), or a [`ROLES_ENV`] of
/// whitespace alone, which names no role. The message starts with the
/// variable at fault.
pub fn compose_roles(
    judge_roles: Option<&str>,
    api_interfaces: Option<&str>,
) -> Result<(Roles, ComposeOrigin)> {
    let set = |v: Option<&str>| v.filter(|v| !v.is_empty()).map(str::to_owned);
    let usage = "the roles are --discord --api --web --mcp --jobs";
    if let Some(flags) = set(judge_roles) {
        let roles = parse(flags.split_whitespace(), ROLES_ENV, usage)?;
        return roles
            .map(|r| (r, ComposeOrigin::Roles))
            .ok_or_else(|| anyhow::anyhow!("{ROLES_ENV}: names no role\n\n{usage}"));
    }
    let (interfaces, origin) = match set(api_interfaces) {
        Some(v) => (v, ComposeOrigin::ApiInterfaces),
        None => ("--api --web".to_owned(), ComposeOrigin::Default),
    };
    let flags = format!("--discord --jobs {interfaces}");
    let roles = parse(flags.split_whitespace(), API_INTERFACES_ENV, usage)?;
    // `--discord --jobs` are always there, so this is never `None`.
    roles
        .map(|r| (r, origin))
        .ok_or_else(|| anyhow::anyhow!("{API_INTERFACES_ENV}: names no role\n\n{usage}"))
}

/// The warning a `judgebot` launch logs while [`API_INTERFACES_ENV`] is set
/// (`.env` reaches the container whole, so the compose service sees it):
/// the variable is deprecated, and the warning names the [`ROLES_ENV`] line
/// that replaces it. `roles` are the ones this launch runs.
#[must_use]
pub fn api_interfaces_warning(
    api_interfaces: Option<&str>,
    judge_roles: Option<&str>,
    roles: &Roles,
) -> Option<String> {
    // Set as Compose reads `:-`: anything but empty, whitespace included.
    let set = |v: Option<&str>| v.is_some_and(|v| !v.is_empty());
    let api = api_interfaces.filter(|v| !v.is_empty())?;
    Some(if set(judge_roles) {
        format!(
            "{API_INTERFACES_ENV} is deprecated and {ROLES_ENV} overrides it: \
             remove {API_INTERFACES_ENV} from .env"
        )
    } else {
        let blank = if api.trim().is_empty() {
            " It names no interface, so the compose service runs no --api, --web or \
             --mcp role and serves no page."
        } else {
            ""
        };
        format!(
            "{API_INTERFACES_ENV} is deprecated, and a later release stops reading it: \
             replace it in .env with {ROLES_ENV}='{}', the roles this process runs.{blank}",
            roles.flags()
        )
    })
}

/// Where a launch's roles came from, for the startup log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Flags on the command line (they win over [`ROLES_ENV`]).
    CommandLine,
    /// [`ROLES_ENV`].
    Environment,
    /// Fixed by the compatibility name the binary was invoked as.
    Name,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CommandLine => "command line",
            Self::Environment => ROLES_ENV,
            Self::Name => "program name",
        })
    }
}

/// The Discord role's requirements, met.
#[derive(Debug)]
pub struct Discord {
    /// The adapter's settings, the token among them.
    pub cfg: discord::Config,
    /// The contact `/help` and `/license` name.
    pub operator: DiscordOperator,
}

/// The adapters a process serves, at least one of them.
#[derive(Debug)]
pub enum Adapters {
    /// The Discord bot alone.
    Discord(Discord),
    /// The HTTP interfaces alone.
    Network(Network),
    /// Both, concurrently, over one composition.
    Both(Discord, Network),
}

/// The serving roles and the models they share: one meter for all of them.
#[derive(Debug)]
pub struct Serve {
    /// What is served.
    pub adapters: Adapters,
    /// The models, built from the configuration before anything connected.
    pub models: Models,
}

/// A launch whose every role has what it requires.
#[derive(Debug)]
pub struct Plan {
    /// The serving roles, if any.
    pub serve: Option<Serve>,
    /// The scheduled jobs, if `--jobs` was given.
    pub jobs: Option<Jobs>,
}

impl Plan {
    /// Connections for the process's shared pool: what the bot and the API
    /// each had, summed. The scheduler opens its own.
    #[must_use]
    pub fn pool_size(&self) -> u32 {
        match self.serve.as_ref().map(|s| &s.adapters) {
            None => 2,
            Some(Adapters::Discord(_) | Adapters::Network(_)) => 5,
            Some(Adapters::Both(..)) => 10,
        }
    }
}

/// One unmet requirement, under the role flags it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The flags of the roles that require it.
    pub roles: String,
    /// What is missing or wrong.
    pub cause: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.roles, self.cause)
    }
}

/// Every requirement a launch does not meet: at least one.
#[derive(Debug)]
pub struct Unstartable {
    /// The launch's role flags.
    pub roles: String,
    /// What it lacks.
    pub problems: NonEmpty<Problem>,
}

impl fmt::Display for Unstartable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot start {}:", self.roles)?;
        for p in &self.problems {
            write!(f, "\n  {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Unstartable {}

/// Check every role's requirements and assemble the [`Plan`]. Every unmet
/// requirement is reported at once, each under the role flags it belongs to.
/// Nothing here connects or binds.
///
/// * `--discord`: `DISCORD_TOKEN` and `JUDGE_OPERATOR_DISCORD`.
/// * `--api`, `--web`, `--mcp`: `JUDGE_OPERATOR_EMAIL`; `--web` a built page
///   in `WEB_DIST`, `--mcp` an `MCP_TOKEN` ([`ApiConfig::check`]).
/// * Any of those (the serving roles): models that build ([`JudgeConfig::models`]).
/// * `--jobs`: a schedule that is on, when it is the only role. With the
///   schedule off a jobs-only process would have nothing to do.
///
/// # Errors
/// [`Unstartable`]: any requirement unmet, or a malformed variable a role
/// reads.
pub fn plan(
    roles: &Roles,
    judge: &JudgeConfig,
    vars: &dyn Fn(&str) -> Option<String>,
) -> Result<Plan, Unstartable> {
    let mut problems: Vec<Problem> = vec![];
    let mut fail = |roles: &str, causes: Vec<String>| {
        problems.extend(causes.into_iter().map(|cause| Problem {
            roles: roles.to_owned(),
            cause,
        }));
    };
    let mut discord = None;
    let mut interfaces: Vec<Interface> = vec![];
    let mut jobs = None;
    for role in roles.iter() {
        match role {
            Role::Discord => match discord_role(judge, vars) {
                Ok(d) => discord = Some(d),
                Err(causes) => fail(Role::Discord.flag(), causes),
            },
            Role::Api => interfaces.push(Interface::Api),
            Role::Web => interfaces.push(Interface::Web),
            Role::Mcp => interfaces.push(Interface::Mcp),
            Role::Jobs => jobs = Some(judge.jobs()),
        }
    }
    let serving = roles
        .iter()
        .filter(|r| r.serves())
        .map(Role::flag)
        .collect::<Vec<_>>()
        .join(" ");
    let network = match NonEmpty::from_vec(interfaces) {
        None => None,
        Some(interfaces) => {
            let interfaces = Interfaces::of(&interfaces);
            let flags = interfaces
                .iter()
                .map(Interface::flag)
                .collect::<Vec<_>>()
                .join(" ");
            match network_roles(judge, vars, interfaces) {
                Ok(n) => Some(n),
                Err(causes) => {
                    fail(&flags, causes);
                    None
                }
            }
        }
    };
    // The models are pure configuration (a chat model named, a price for
    // it), so a mistake there is reported here with the rest, before the
    // database is touched. Only the serving roles call a model.
    let models = if serving.is_empty() {
        None
    } else {
        match judge.models() {
            Ok(m) => Some(m),
            Err(e) => {
                fail(&serving, vec![e.to_string()]);
                None
            }
        }
    };
    if serving.is_empty() && jobs.as_ref().is_some_and(|j| j.refresh == Schedule::Off) {
        fail(
            Role::Jobs.flag(),
            vec![
                "the only role, with the schedule off (JUDGE_REFRESH_HOURS=0), has nothing to \
                 do: set JUDGE_REFRESH_HOURS or add a serving role"
                    .to_owned(),
            ],
        );
    }
    if let Some(problems) = NonEmpty::from_vec(problems) {
        return Err(Unstartable {
            roles: roles.flags(),
            problems,
        });
    }
    let adapters = match (discord, network) {
        (None, None) => None,
        (Some(d), None) => Some(Adapters::Discord(d)),
        (None, Some(n)) => Some(Adapters::Network(n)),
        (Some(d), Some(n)) => Some(Adapters::Both(d, n)),
    };
    // With no problems, a serving role means both its adapter and the models.
    let serve = adapters
        .zip(models)
        .map(|(adapters, models)| Serve { adapters, models });
    Ok(Plan { serve, jobs })
}

/// `--discord`'s requirements: the token (with the adapter's other
/// settings) and the operator's Discord username.
fn discord_role(
    judge: &JudgeConfig,
    vars: &dyn Fn(&str) -> Option<String>,
) -> Result<Discord, Vec<String>> {
    let cfg = discord::Config::from_vars(vars).map_err(|e| format!("{e:#}"));
    let operator = judge.discord_operator().map_err(|e| e.to_string());
    // No token at all is most often a deployment that never meant to run the
    // bot: the compose service's default roles include --discord, so an
    // upgrade from the two-service file lands here. Say how to leave it out.
    let no_token = vars("DISCORD_TOKEN").is_none_or(|t| t.trim().is_empty());
    let hint = no_token.then(|| {
        format!(
            "to run without Discord, leave --discord out of the roles: \
             {ROLES_ENV}='--api --web --jobs' in .env (the compose service's default \
             includes --discord)"
        )
    });
    match (cfg, operator) {
        (Ok(cfg), Ok(operator)) => Ok(Discord { cfg, operator }),
        (cfg, operator) => Err(cfg
            .err()
            .into_iter()
            .chain(operator.err())
            .chain(hint)
            .collect()),
    }
}

/// The network roles' requirements: the API settings, the operator's support
/// address, and every interface's own ([`ApiConfig::check`]), all of them
/// reported, not the first.
fn network_roles(
    judge: &JudgeConfig,
    vars: &dyn Fn(&str) -> Option<String>,
    interfaces: Interfaces,
) -> Result<Network, Vec<String>> {
    let mut causes = vec![];
    let cfg = match ApiConfig::from_vars(vars) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            causes.push(format!("{e:#}"));
            None
        }
    };
    let operator = match judge.network_operator() {
        Ok(o) => Some(o),
        Err(e) => {
            causes.push(e.to_string());
            None
        }
    };
    if let Some(cfg) = &cfg
        && let Err(Refused(unmet)) = cfg.check(&interfaces)
    {
        causes.extend(unmet.iter().map(ToString::to_string));
    }
    match (cfg, operator) {
        (Some(cfg), Some(operator)) if causes.is_empty() => Network::new(cfg, interfaces, operator)
            .map_err(|Refused(unmet)| unmet.iter().map(ToString::to_string).collect()),
        _ => Err(causes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn roles(flags: &str) -> Option<Roles> {
        parse(flags.split_whitespace(), "test", "usage")
            .ok()
            .flatten()
    }

    /// Plan `flags` against an environment, the configuration loaded from
    /// the same variables.
    fn plan_bare(flags: &str, pairs: &[(&str, &str)]) -> anyhow::Result<Plan> {
        let get = vars(pairs);
        let judge = JudgeConfig::from_vars(&get)?;
        let roles = roles(flags).ok_or_else(|| anyhow::anyhow!("no roles in {flags:?}"))?;
        Ok(plan(&roles, &judge, &get)?)
    }

    /// [`plan_bare`] with a chat model configured (never called).
    fn plan_with(flags: &str, pairs: &[(&str, &str)]) -> anyhow::Result<Plan> {
        let mut pairs = pairs.to_vec();
        pairs.push(KEY);
        plan_bare(flags, &pairs)
    }

    const KEY: (&str, &str) = ("ANTHROPIC_API_KEY", "sk-test-never-sent");
    const TOKEN: (&str, &str) = ("DISCORD_TOKEN", "a-bot-token");
    const DISCORD_OP: (&str, &str) = ("JUDGE_OPERATOR_DISCORD", "someone");
    const EMAIL: (&str, &str) = ("JUDGE_OPERATOR_EMAIL", "help@example.com");
    const MCP: (&str, &str) = ("MCP_TOKEN", "0123456789abcdef0123456789abcdef");

    fn error(r: anyhow::Result<Plan>) -> String {
        r.err().map(|e| format!("{e:#}")).unwrap_or_default()
    }

    /// The problems a launch reports, as `Unstartable` holds them.
    fn problems(flags: &str, pairs: &[(&str, &str)]) -> Vec<Problem> {
        let get = vars(pairs);
        let Ok(judge) = JudgeConfig::from_vars(&get) else {
            return vec![];
        };
        let Some(roles) = roles(flags) else {
            return vec![];
        };
        match plan(&roles, &judge, &get) {
            Ok(_) => vec![],
            Err(u) => u.problems.into_iter().collect(),
        }
    }

    #[test]
    fn roles_dedupe_into_one_order() {
        let r = roles("--jobs --api --discord");
        assert_eq!(
            r.as_ref().map(ToString::to_string),
            Some("discord, api, jobs".to_owned())
        );
        assert_eq!(
            r.as_ref().map(Roles::flags),
            Some("--discord --api --jobs".to_owned())
        );
        assert_eq!(r.as_ref().map(Roles::off), Some("web, mcp".to_owned()));
        assert_eq!(r, roles("--discord --api --jobs"));
    }

    #[test]
    fn no_flags_is_no_roles_and_bad_flags_are_refused() {
        assert!(matches!(parse(Vec::<&str>::new(), "t", "u"), Ok(None)));
        for (args, needle) in [
            ("--discord --discord", "--discord given more than once"),
            ("--bot", "\"--bot\""),
            ("discord", "\"discord\""),
            ("--help", "\"--help\""),
        ] {
            let r = parse(args.split_whitespace(), "JUDGE_ROLES", "the usage");
            assert!(
                r.as_ref().is_err_and(|e| {
                    let text = format!("{e:#}");
                    text.contains(needle)
                        && text.contains("JUDGE_ROLES")
                        && text.contains("the usage")
                }),
                "{args:?}: {r:?}"
            );
        }
    }

    #[test]
    fn every_role_has_a_distinct_flag() {
        for r in Role::ALL {
            assert!(r.flag().starts_with("--"), "{r:?}");
            assert_eq!(Role::from_flag(r.flag()), Some(r));
        }
        let flags: std::collections::BTreeSet<&str> =
            Role::ALL.into_iter().map(Role::flag).collect();
        assert_eq!(flags.len(), Role::ALL.len());
        for i in Interface::ALL {
            assert_eq!(
                Role::from(i).flag(),
                i.flag(),
                "a network role is its interface's flag"
            );
            assert!(Role::from(i).serves());
            assert_eq!(Role::from(i).interface(), Some(i));
        }
        assert!(Role::Discord.serves() && !Role::Jobs.serves());
        assert_eq!(Role::Discord.interface(), None);
        assert_eq!(Role::Jobs.interface(), None);
    }

    #[test]
    fn judge_api_was_its_interfaces_and_the_jobs() {
        use nonempty::nonempty;
        let r = Roles::api_compat(&Interfaces::of(&nonempty![Interface::Web, Interface::Api]));
        assert_eq!(r.flags(), "--api --web --jobs");
    }

    #[test]
    fn discord_needs_its_token_and_its_operator() {
        let text = error(plan_with("--discord", &[]));
        assert!(
            text.contains("--discord: DISCORD_TOKEN") && text.contains("JUDGE_OPERATOR_DISCORD"),
            "{text}"
        );
        assert!(
            text.contains("JUDGE_ROLES='--api --web --jobs'"),
            "no token names the way to leave Discord out: {text}"
        );
        let text = error(plan_with("--discord", &[TOKEN]));
        assert!(
            !text.contains("DISCORD_TOKEN")
                && !text.contains("JUDGE_ROLES")
                && text.contains("JUDGE_OPERATOR_DISCORD"),
            "{text}"
        );
        let ok = plan_with("--discord", &[TOKEN, DISCORD_OP]);
        assert!(
            matches!(
                ok,
                Ok(Plan {
                    serve: Some(Serve {
                        adapters: Adapters::Discord(_),
                        ..
                    }),
                    jobs: None
                })
            ),
            "{ok:?}"
        );
    }

    #[test]
    fn the_network_roles_need_the_support_address() {
        for flags in ["--api", "--mcp", "--api --web"] {
            let text = error(plan_with(flags, &[MCP]));
            assert!(text.contains("JUDGE_OPERATOR_EMAIL"), "{flags}: {text}");
        }
        let ok = plan_with("--api", &[EMAIL]);
        assert!(
            matches!(
                ok,
                Ok(Plan {
                    serve: Some(Serve {
                        adapters: Adapters::Network(_),
                        ..
                    }),
                    jobs: None
                })
            ),
            "{ok:?}"
        );
    }

    #[test]
    fn mcp_needs_its_token_and_web_its_page() {
        let text = error(plan_with("--api --mcp", &[EMAIL]));
        assert!(
            text.contains("--api --mcp: --mcp was given but MCP_TOKEN"),
            "{text}"
        );
        let text = error(plan_with(
            "--web",
            &[EMAIL, ("WEB_DIST", "no-such-directory")],
        ));
        assert!(text.contains("index.html"), "{text}");
    }

    /// A missing address, a missing page and a missing token, all at once.
    #[test]
    fn every_network_requirement_is_reported_at_once() {
        let found = problems("--web --mcp", &[KEY, ("WEB_DIST", "no-such-directory")]);
        let causes: Vec<&str> = found.iter().map(|p| p.cause.as_str()).collect();
        assert_eq!(found.len(), 3, "{causes:#?}");
        assert!(found.iter().all(|p| p.roles == "--web --mcp"), "{found:?}");
        for needle in ["JUDGE_OPERATOR_EMAIL", "index.html", "MCP_TOKEN"] {
            assert!(
                causes.iter().any(|c| c.contains(needle)),
                "{needle} missing: {causes:#?}"
            );
        }
    }

    #[test]
    fn every_unmet_requirement_is_reported_at_once() {
        let text = error(plan_bare("--discord --api --mcp --jobs", &[]));
        for needle in [
            "cannot start --discord --api --mcp --jobs",
            "DISCORD_TOKEN",
            "JUDGE_OPERATOR_DISCORD",
            "JUDGE_OPERATOR_EMAIL",
            "MCP_TOKEN",
            "--discord --api --mcp: no model configured",
        ] {
            assert!(text.contains(needle), "{needle} missing: {text}");
        }
    }

    /// The models are checked before anything connects, and only for a
    /// role that calls one.
    #[test]
    fn the_serving_roles_need_a_model_and_the_jobs_do_not() {
        let text = error(plan_bare("--api", &[EMAIL]));
        assert!(text.contains("--api: no model configured"), "{text}");
        let ok = plan_bare("--jobs", &[]);
        assert!(
            matches!(
                &ok,
                Ok(Plan {
                    serve: None,
                    jobs: Some(_)
                })
            ),
            "{ok:?}"
        );
    }

    #[test]
    fn jobs_need_nothing_more_and_serve_nothing() {
        let ok = plan_with("--jobs", &[]);
        assert!(
            matches!(
                &ok,
                Ok(Plan {
                    serve: None,
                    jobs: Some(_)
                })
            ),
            "{ok:?}"
        );
        assert_eq!(ok.as_ref().map(Plan::pool_size).ok(), Some(2));
    }

    #[test]
    fn jobs_alone_with_the_schedule_off_are_refused() {
        let off = ("JUDGE_REFRESH_HOURS", "0");
        let text = error(plan_with("--jobs", &[off]));
        assert!(
            text.contains("--jobs: the only role, with the schedule off"),
            "{text}"
        );
        // Beside a serving role the jobs are merely off.
        let ok = plan_with("--api --jobs", &[off, EMAIL]);
        assert!(
            matches!(&ok, Ok(Plan { jobs: Some(j), .. }) if j.refresh == Schedule::Off),
            "{ok:?}"
        );
    }

    fn compose(judge_roles: Option<&str>, api: Option<&str>) -> Option<(String, ComposeOrigin)> {
        compose_roles(judge_roles, api)
            .ok()
            .map(|(r, o)| (r.flags(), o))
    }

    /// The three cases an upgrade meets: a `.env` with neither variable
    /// (every install before roles), one with the deprecated
    /// `API_INTERFACES`, and one with `JUDGE_ROLES`.
    #[test]
    fn the_compose_roles_keep_every_older_env_working() {
        let all_but_mcp = "--discord --api --web --jobs".to_owned();
        for blank in [None, Some("")] {
            assert_eq!(
                compose(blank, blank),
                Some((all_but_mcp.clone(), ComposeOrigin::Default))
            );
        }
        assert_eq!(
            compose(None, Some("--api --mcp")),
            Some((
                "--discord --api --mcp --jobs".to_owned(),
                ComposeOrigin::ApiInterfaces
            ))
        );
        assert_eq!(
            compose(Some(""), Some("--web")),
            Some((
                "--discord --web --jobs".to_owned(),
                ComposeOrigin::ApiInterfaces
            ))
        );
        // Whitespace is not empty to Compose's `:-`: no interfaces at all.
        assert_eq!(
            compose(None, Some("  ")),
            Some(("--discord --jobs".to_owned(), ComposeOrigin::ApiInterfaces))
        );
        for api in [None, Some("--api --mcp")] {
            assert_eq!(
                compose(Some("--api --web --jobs"), api),
                Some(("--api --web --jobs".to_owned(), ComposeOrigin::Roles))
            );
        }
    }

    #[test]
    fn a_bad_compose_role_list_names_its_variable() {
        for (roles, api, needle) in [
            (
                Some("--bot"),
                None,
                "JUDGE_ROLES: unknown argument \"--bot\"",
            ),
            (Some("   "), None, "JUDGE_ROLES: names no role"),
            (
                None,
                Some("--discord"),
                "API_INTERFACES: --discord given more than once",
            ),
            (
                None,
                Some("--jobs"),
                "API_INTERFACES: --jobs given more than once",
            ),
            (
                None,
                Some("--help"),
                "API_INTERFACES: unknown argument \"--help\"",
            ),
        ] {
            let r = compose_roles(roles, api);
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").starts_with(needle)),
                "{roles:?} {api:?}: {r:?}"
            );
        }
    }

    /// `docker-compose.yml` hands the binary its roles as a string nobody
    /// else checks: renaming a flag, or the file drifting from
    /// [`compose_roles`], would leave every other test green.
    #[test]
    fn roles_match_the_compose_file() {
        let compose = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker-compose.yml"),
        )
        .unwrap_or_default();
        let commands: Vec<&str> = compose
            .lines()
            .filter_map(|l| l.trim().strip_prefix("command: "))
            .collect();
        assert!(
            commands.contains(&COMPOSE_COMMAND),
            "no service passes {COMPOSE_COMMAND:?}: {commands:?}"
        );
        // The literal parts are role flags this binary accepts.
        let literal = COMPOSE_COMMAND
            .replace("${JUDGE_ROLES:-", "")
            .replace("${API_INTERFACES:-", "")
            .replace('}', "");
        let r = parse(literal.split_whitespace(), "compose", "u");
        assert_eq!(
            r.ok().flatten().map(|r| r.flags()),
            Some("--discord --api --web --jobs".to_owned())
        );
    }

    #[test]
    fn api_interfaces_warns_and_names_its_replacement() -> Result<(), &'static str> {
        let r = roles("--discord --api --mcp --jobs").ok_or("roles")?;
        assert_eq!(api_interfaces_warning(None, None, &r), None);
        assert_eq!(api_interfaces_warning(Some(""), None, &r), None);
        // Whitespace is set to Compose, and leaves the page off.
        let w = api_interfaces_warning(Some(" "), None, &r);
        assert!(
            w.as_ref()
                .is_some_and(|w| w.contains("deprecated") && w.contains("serves no page")),
            "{w:?}"
        );
        let w = api_interfaces_warning(Some("--api --mcp"), None, &r);
        assert!(
            w.as_ref().is_some_and(|w| w.contains("deprecated")
                && w.contains("JUDGE_ROLES='--discord --api --mcp --jobs'")),
            "{w:?}"
        );
        let w = api_interfaces_warning(Some("--api"), Some("--api --jobs"), &r);
        assert!(
            w.as_ref()
                .is_some_and(|w| w.contains("JUDGE_ROLES overrides it")
                    && w.contains("remove API_INTERFACES")),
            "{w:?}"
        );
        Ok(())
    }

    #[test]
    fn discord_and_the_network_roles_share_one_process() {
        let ok = plan_with("--discord --api --jobs", &[TOKEN, DISCORD_OP, EMAIL]);
        assert!(
            matches!(
                &ok,
                Ok(Plan {
                    serve: Some(Serve {
                        adapters: Adapters::Both(..),
                        ..
                    }),
                    jobs: Some(_)
                })
            ),
            "{ok:?}"
        );
        assert_eq!(ok.as_ref().map(Plan::pool_size).ok(), Some(10));
    }
}
