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

/// The variable that chose the `api` service's interfaces in the 1.x compose
/// file. Retired in 2.0.0: nothing reads it, and a launch that finds it set
/// is refused ([`refuse_api_interfaces`]) rather than started without the
/// roles it named.
pub const API_INTERFACES_ENV: &str = "API_INTERFACES";

/// The `command:` of `docker-compose.yml`'s `judgebot` service, verbatim
/// (`roles_match_the_compose_file` holds the file to it). Compose
/// interpolates it before the binary starts: [`ROLES_ENV`] when it is set and
/// not empty, else every role but `--mcp`. [`compose_roles`] is the same rule
/// in Rust.
pub const COMPOSE_COMMAND: &str = "${JUDGE_ROLES:---discord --api --web --jobs}";

/// What decided the roles [`compose_roles`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeOrigin {
    /// [`ROLES_ENV`].
    Roles,
    /// It is unset: every role but `--mcp`.
    Default,
}

impl fmt::Display for ComposeOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Roles => ROLES_ENV,
            Self::Default => "the compose file's default",
        })
    }
}

/// The roles the compose `judgebot` service runs, given the variables'
/// values in `.env`: [`COMPOSE_COMMAND`] as Compose interpolates it (`:-`,
/// so an empty value counts as unset) and the binary then reads it.
///
/// # Errors
/// What the binary would refuse: an argument that is not a role flag, a
/// role named twice, a [`ROLES_ENV`] of whitespace alone, which names no
/// role, or a set [`API_INTERFACES_ENV`]. The message starts with the
/// variable at fault.
pub fn compose_roles(
    judge_roles: Option<&str>,
    api_interfaces: Option<&str>,
) -> Result<(Roles, ComposeOrigin)> {
    refuse_api_interfaces(api_interfaces, judge_roles, false)?;
    let usage = "the roles are --discord --api --web --mcp --jobs";
    let Some(flags) = judge_roles.filter(|v| !v.is_empty()) else {
        let all_but_mcp = nonempty::nonempty![Role::Discord, Role::Api, Role::Web, Role::Jobs];
        return Ok((Roles::of(&all_but_mcp), ComposeOrigin::Default));
    };
    parse(flags.split_whitespace(), ROLES_ENV, usage)?
        .map(|r| (r, ComposeOrigin::Roles))
        .ok_or_else(|| anyhow::anyhow!("{ROLES_ENV}: names no role\n\n{usage}"))
}

/// Refuse a launch while [`API_INTERFACES_ENV`] is set (anything but empty,
/// as Compose reads `:-`). A 1.x `.env` chose the page's interfaces with it;
/// 2.0.0 reads only [`ROLES_ENV`], so starting anyway would quietly drop the
/// interfaces it named. The error names the line that replaces it, or says
/// to remove it when the roles are already named: by [`ROLES_ENV`], or by
/// the command line (`named_elsewhere`).
///
/// # Errors
/// The variable is set.
pub fn refuse_api_interfaces(
    api_interfaces: Option<&str>,
    judge_roles: Option<&str>,
    named_elsewhere: bool,
) -> Result<()> {
    let Some(api) = api_interfaces.filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    // Whitespace names no role, so the replacement line is the fix for both.
    let named = if named_elsewhere {
        Some("the command line")
    } else {
        judge_roles
            .is_some_and(|v| !v.trim().is_empty())
            .then_some(ROLES_ENV)
    };
    if let Some(by) = named {
        anyhow::bail!(
            "{API_INTERFACES_ENV}: no longer read since 2.0.0, and {by} already names the \
             roles: remove {API_INTERFACES_ENV} from .env"
        );
    }
    let roles = format!("--discord --jobs {}", api.trim());
    let replacement = parse(roles.split_whitespace(), API_INTERFACES_ENV, "")
        .ok()
        .flatten()
        .map_or_else(|| "--discord --api --web --jobs".to_owned(), |r| r.flags());
    anyhow::bail!(
        "{API_INTERFACES_ENV}: no longer read since 2.0.0: replace it in .env with \
         {ROLES_ENV}='{replacement}'"
    )
}

/// Where a launch's roles came from, for the startup log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Flags on the command line (they win over [`ROLES_ENV`]).
    CommandLine,
    /// [`ROLES_ENV`].
    Environment,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CommandLine => "command line",
            Self::Environment => ROLES_ENV,
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
        let interfaces: Vec<Interface> = Role::ALL
            .into_iter()
            .filter_map(|r| {
                let i = r.interface()?;
                assert_eq!(r.flag(), i.flag(), "a network role is its interface's flag");
                assert!(r.serves());
                Some(i)
            })
            .collect();
        assert_eq!(interfaces, Interface::ALL, "every interface is a role");
        assert!(Role::Discord.serves() && !Role::Jobs.serves());
        assert_eq!(Role::Discord.interface(), None);
        assert_eq!(Role::Jobs.interface(), None);
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

    #[test]
    fn the_compose_roles_are_judge_roles_or_every_role_but_mcp() {
        for blank in [None, Some("")] {
            assert_eq!(
                compose(blank, blank),
                Some((
                    "--discord --api --web --jobs".to_owned(),
                    ComposeOrigin::Default
                ))
            );
        }
        assert_eq!(
            compose(Some("--api --web --jobs"), Some("")),
            Some(("--api --web --jobs".to_owned(), ComposeOrigin::Roles))
        );
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
            (None, Some("--api --mcp"), "API_INTERFACES: no longer read"),
            (
                Some("--api"),
                Some("--api"),
                "API_INTERFACES: no longer read",
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

    /// A 1.x `.env` that still sets `API_INTERFACES` is refused with the
    /// line that replaces it, not started without the roles it named.
    #[test]
    fn api_interfaces_is_refused_with_its_replacement() {
        assert!(refuse_api_interfaces(None, None, false).is_ok());
        assert!(refuse_api_interfaces(Some(""), Some("--api"), true).is_ok());
        let text = |api, roles| {
            refuse_api_interfaces(api, roles, false)
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        };
        let t = text(Some("--api --mcp"), None);
        assert!(
            t.contains("JUDGE_ROLES='--discord --api --mcp --jobs'"),
            "{t}"
        );
        // Whitespace is set to Compose's `:-`, and named no interface.
        let t = text(Some(" "), None);
        assert!(t.contains("JUDGE_ROLES='--discord --jobs'"), "{t}");
        // A value the old file would have refused gets the default line.
        let t = text(Some("--discord"), None);
        assert!(
            t.contains("JUDGE_ROLES='--discord --api --web --jobs'"),
            "{t}"
        );
        let t = text(Some("--api"), Some("--api --jobs"));
        assert!(t.contains("remove API_INTERFACES"), "{t}");
        // A blank JUDGE_ROLES names nothing: the replacement line fixes both.
        let t = text(Some("--api"), Some("  "));
        assert!(t.contains("JUDGE_ROLES='--discord --api --jobs'"), "{t}");
        // Roles on the command line: JUDGE_ROLES is not read, so no line.
        let t = refuse_api_interfaces(Some("--api"), None, true)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            t.contains("the command line already names") && !t.contains("JUDGE_ROLES"),
            "{t}"
        );
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
            .replace('}', "");
        let r = parse(literal.split_whitespace(), "compose", "u");
        assert_eq!(
            r.ok().flatten().map(|r| r.flags()),
            Some("--discord --api --web --jobs".to_owned())
        );
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
