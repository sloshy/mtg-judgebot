//! The command line: which program the binary was invoked as, and what it
//! was asked to do. Pure, so the dispatch is tested without a process.
//!
//! `judgebot` answers to four names. Its own takes role flags or `ingest`;
//! the other three are the binaries it replaced, kept as links to it in the
//! image so a compose file or a script written for them keeps working:
//!
//! | invoked as | runs |
//! |---|---|
//! | `judgebot --discord --api …` | those roles (none: [`ROLES_ENV`]) |
//! | `judgebot ingest <cmd> …` | the ingest command line |
//! | `judge-bot` | `judgebot --discord --jobs` |
//! | `judge-api [--api] [--web] [--mcp]` | those interfaces (none: `--api`) and `--jobs` |
//! | `judge-ingest <cmd> …` | `judgebot ingest <cmd> …` |
//!
//! A compatibility name ignores [`ROLES_ENV`]: an old compose file runs one
//! container per name from one `.env`, and reading the variable there would
//! start every role in each of them.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::Path,
};

use anyhow::Result;
use judge_api::{Launch, interfaces};

use crate::roles::{self, Origin, ROLES_ENV, Role, Roles};

/// The name the binary was invoked as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Name {
    /// `judgebot`, or any name that is not one of the others.
    Judgebot,
    /// `judge-bot`: the Discord bot and the jobs.
    JudgeBot,
    /// `judge-api`: the HTTP interfaces and the jobs.
    JudgeApi,
    /// `judge-ingest`: the ingest command line.
    JudgeIngest,
}

impl Name {
    /// The compatibility names, each with the program name it answers to.
    const COMPAT: [(Self, &'static str); 3] = [
        (Self::JudgeBot, "judge-bot"),
        (Self::JudgeApi, "judge-api"),
        (Self::JudgeIngest, "judge-ingest"),
    ];

    /// The name `argv0` (a path or a bare name) invokes.
    #[must_use]
    pub fn of(argv0: &OsStr) -> Self {
        let file = Path::new(argv0)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file = file.strip_suffix(".exe").unwrap_or(&file);
        Self::COMPAT
            .into_iter()
            .find_map(|(name, program)| (program == file).then_some(name))
            .unwrap_or(Self::Judgebot)
    }

    /// Its program name.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Judgebot => "judgebot",
            Self::JudgeBot => "judge-bot",
            Self::JudgeApi => "judge-api",
            Self::JudgeIngest => "judge-ingest",
        }
    }

    /// What the process calls itself in the spend alerts, the refresh
    /// record and its database connections: the names the two processes had
    /// before, so a deployment still running both can tell them apart.
    #[must_use]
    pub const fn process(self) -> &'static str {
        match self {
            Self::Judgebot => "judgebot",
            Self::JudgeBot => "bot",
            Self::JudgeApi => "api",
            Self::JudgeIngest => "ingest",
        }
    }

    /// Is this one of the names kept for compatibility?
    #[must_use]
    pub const fn is_compat(self) -> bool {
        match self {
            Self::Judgebot => false,
            Self::JudgeBot | Self::JudgeApi | Self::JudgeIngest => true,
        }
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program())
    }
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Print this and exit successfully.
    Help(String),
    /// The ingest command line, over these arguments.
    Ingest {
        /// The arguments after `ingest` (or after `judge-ingest`).
        args: Vec<OsString>,
        /// The name invoked.
        name: Name,
    },
    /// Run roles.
    Serve {
        /// The roles, or `None` to read them from [`ROLES_ENV`].
        roles: Option<(Roles, Origin)>,
        /// The name invoked.
        name: Name,
    },
}

/// The usage `judgebot --help` prints.
pub const USAGE: &str = "\
usage: judgebot <role>...
       judgebot ingest <command> [args]   (judgebot ingest --help)

Roles: at least one, named on the command line or, with none there, in
JUDGE_ROLES (the same flags separated by spaces).

  --discord  the Discord bot (requires DISCORD_TOKEN and JUDGE_OPERATOR_DISCORD)
  --api      POST /api/judge, the anonymous question route
  --web      the built web page, from WEB_DIST (requires its index.html)
  --mcp      the MCP transport at /mcp (requires MCP_TOKEN)
  --jobs     the scheduled data refresh (JUDGE_REFRESH_HOURS; 0 turns it off)

--api, --web and --mcp require JUDGE_OPERATOR_EMAIL and listen on API_ADDR,
where GET /api/health and GET /api/about are served whichever of them is on.
Everything else is configured through the environment; see .env.example.";

/// Parse the program name and the arguments after it.
///
/// Takes [`OsString`]s, because `std::env::args()` panics on an argument
/// that is not UTF-8: a mistyped byte gets the usage like any other unknown
/// argument.
///
/// # Errors
/// An unknown argument or a role named twice, with the usage; any argument
/// to `judge-bot`.
pub fn parse(argv0: &OsStr, args: impl IntoIterator<Item = OsString>) -> Result<Invocation> {
    let name = Name::of(argv0);
    let args: Vec<OsString> = args.into_iter().collect();
    let help = |a: &OsString| a == "-h" || a == "--help";
    match name {
        Name::JudgeIngest => Ok(Invocation::Ingest { args, name }),
        Name::JudgeBot => match args.first() {
            None => Ok(Invocation::Serve {
                roles: Some((judge_bot_roles(), Origin::Name)),
                name,
            }),
            Some(a) if help(a) => Ok(Invocation::Help(judge_bot_usage())),
            Some(a) => anyhow::bail!(
                "judge-bot takes no arguments (got {:?})\n\n{}",
                a.to_string_lossy(),
                judge_bot_usage()
            ),
        },
        Name::JudgeApi => match interfaces::parse(args)? {
            Launch::Help => Ok(Invocation::Help(judge_api_usage())),
            Launch::Serve(interfaces) => Ok(Invocation::Serve {
                roles: Some((Roles::api_compat(&interfaces), Origin::Name)),
                name,
            }),
        },
        Name::Judgebot => {
            if args.first().is_some_and(|a| a == "ingest") {
                return Ok(Invocation::Ingest {
                    args: args.into_iter().skip(1).collect(),
                    name,
                });
            }
            if args.iter().any(help) {
                return Ok(Invocation::Help(USAGE.to_owned()));
            }
            let flags = args.iter().map(|a| a.to_string_lossy());
            Ok(Invocation::Serve {
                roles: roles::parse(flags, "command line", USAGE)?
                    .map(|r| (r, Origin::CommandLine)),
                name,
            })
        }
    }
}

/// The roles of a launch that named none on the command line: [`ROLES_ENV`]'s
/// value (blank counts as unset).
///
/// # Errors
/// Neither names a role, or the variable holds something other than role
/// flags.
pub fn resolve(roles: Option<(Roles, Origin)>, env: Option<&str>) -> Result<(Roles, Origin)> {
    if let Some(roles) = roles {
        return Ok(roles);
    }
    let from_env = roles::parse(env.unwrap_or_default().split_whitespace(), ROLES_ENV, USAGE)?;
    from_env.map(|r| (r, Origin::Environment)).ok_or_else(|| {
        anyhow::anyhow!(
            "no roles: name at least one on the command line or in {ROLES_ENV} \
             (for example `judgebot --discord --api --web --jobs`)\n\n{USAGE}"
        )
    })
}

/// What `judge-bot` always ran.
fn judge_bot_roles() -> Roles {
    Roles::of(&nonempty::nonempty![Role::Discord, Role::Jobs])
}

fn judge_bot_usage() -> String {
    format!(
        "usage: judge-bot\n\n\
         A compatibility name for `judgebot {}`, to be removed in a later release.\n\n{USAGE}",
        judge_bot_roles().flags()
    )
}

fn judge_api_usage() -> String {
    format!(
        "{}\n\nA compatibility name for `judgebot` with those interfaces as roles and --jobs, \
         to be removed in a later release.\n\n{USAGE}",
        interfaces::USAGE
    )
}

/// The warning a compatibility name logs at startup, naming the command that
/// replaces it; `None` for `judgebot` itself.
#[must_use]
pub fn compat_warning(name: Name, replacement: &str) -> Option<String> {
    name.is_compat().then(|| {
        format!(
            "invoked as {name}, a compatibility name that a later release removes: \
             run `{replacement}` instead"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(argv0: &str, args: &[&str]) -> Result<Invocation> {
        parse(OsStr::new(argv0), args.iter().map(OsString::from))
    }

    fn served(argv0: &str, args: &[&str]) -> Option<(String, Origin, Name)> {
        match run(argv0, args) {
            Ok(Invocation::Serve {
                roles: Some((r, o)),
                name,
            }) => Some((r.flags(), o, name)),
            _ => None,
        }
    }

    #[test]
    fn the_program_name_is_read_from_any_path() {
        for (argv0, name) in [
            ("judgebot", Name::Judgebot),
            ("/usr/local/bin/judgebot", Name::Judgebot),
            ("target/debug/judgebot", Name::Judgebot),
            ("judge-bot", Name::JudgeBot),
            ("/usr/local/bin/judge-api", Name::JudgeApi),
            ("./judge-ingest", Name::JudgeIngest),
            ("judge-ingest.exe", Name::JudgeIngest),
            ("something-else", Name::Judgebot),
            ("", Name::Judgebot),
        ] {
            assert_eq!(Name::of(OsStr::new(argv0)), name, "{argv0}");
        }
        for (name, program) in Name::COMPAT {
            assert_eq!(name.program(), program);
            assert!(name.is_compat());
        }
        assert!(!Name::Judgebot.is_compat());
    }

    #[test]
    fn judgebot_takes_roles_from_the_command_line_or_leaves_them_to_the_environment() {
        assert_eq!(
            served("judgebot", &["--web", "--discord", "--jobs"]),
            Some((
                "--discord --web --jobs".to_owned(),
                Origin::CommandLine,
                Name::Judgebot
            ))
        );
        assert!(matches!(
            run("judgebot", &[]),
            Ok(Invocation::Serve {
                roles: None,
                name: Name::Judgebot
            })
        ));
        for args in [&["--help"][..], &["-h"], &["--api", "--help"]] {
            assert!(
                matches!(run("judgebot", args), Ok(Invocation::Help(u)) if u == USAGE),
                "{args:?}"
            );
        }
        for args in [
            &["--bot"][..],
            &["--api", "--api"],
            &["serve"],
            &["--api", "ingest"],
        ] {
            let r = run("judgebot", args);
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").contains("usage: judgebot")),
                "{args:?}: {r:?}"
            );
        }
    }

    #[test]
    fn ingest_is_a_subcommand_of_judgebot_and_the_whole_of_judge_ingest() {
        let ingest = |argv0, args: &[&str]| match run(argv0, args) {
            Ok(Invocation::Ingest { args, name }) => Some((args, name)),
            _ => None,
        };
        let os = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            ingest("judgebot", &["ingest", "rules", "latest"]),
            Some((os(&["rules", "latest"]), Name::Judgebot))
        );
        assert_eq!(
            ingest("judgebot", &["ingest", "--help"]),
            Some((os(&["--help"]), Name::Judgebot))
        );
        assert_eq!(
            ingest("judge-ingest", &["rules", "latest"]),
            Some((os(&["rules", "latest"]), Name::JudgeIngest))
        );
        // Not a role flag under that name: the ingest parser refuses it.
        assert_eq!(
            ingest("judge-ingest", &["--api"]),
            Some((os(&["--api"]), Name::JudgeIngest))
        );
    }

    #[test]
    fn judge_bot_is_the_bot_and_the_jobs_and_takes_no_arguments() {
        assert_eq!(
            served("judge-bot", &[]),
            Some(("--discord --jobs".to_owned(), Origin::Name, Name::JudgeBot))
        );
        assert!(matches!(
            run("judge-bot", &["--help"]),
            Ok(Invocation::Help(_))
        ));
        assert!(run("judge-bot", &["--api"]).is_err());
    }

    #[test]
    fn judge_api_keeps_its_interface_rules_and_adds_the_jobs() {
        for (args, roles) in [
            (&[][..], "--api --jobs"),
            (&["--web"], "--web --jobs"),
            (&["--api", "--web"], "--api --web --jobs"),
            (&["--mcp", "--api"], "--api --mcp --jobs"),
        ] {
            assert_eq!(
                served("judge-api", args),
                Some((roles.to_owned(), Origin::Name, Name::JudgeApi)),
                "{args:?}"
            );
        }
        assert!(matches!(run("judge-api", &["-h"]), Ok(Invocation::Help(_))));
        // judge-api never took these.
        for args in [&["--discord"][..], &["--jobs"], &["--web", "--web"]] {
            let r = run("judge-api", args);
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").contains("usage: judge-api")),
                "{args:?}: {r:?}"
            );
        }
    }

    #[test]
    fn the_environment_is_read_only_when_the_command_line_names_nothing() {
        let given = roles::parse(["--api"], "t", "u").ok().flatten();
        let given = given.map(|r| (r, Origin::CommandLine));
        let r = resolve(given.clone(), Some("--discord"));
        assert_eq!(r.ok(), given);

        let r = resolve(None, Some("  --jobs\t--discord \n"));
        assert_eq!(
            r.ok().map(|(r, o)| (r.flags(), o)),
            Some(("--discord --jobs".to_owned(), Origin::Environment))
        );

        for env in [None, Some(""), Some("   ")] {
            let r = resolve(None, env);
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").contains("JUDGE_ROLES")),
                "{env:?}: {r:?}"
            );
        }
        let r = resolve(None, Some("--discord --nope"));
        assert!(
            r.as_ref().is_err_and(|e| {
                let text = format!("{e:#}");
                text.contains("JUDGE_ROLES") && text.contains("--nope")
            }),
            "{r:?}"
        );
    }

    #[test]
    fn only_a_compatibility_name_warns() {
        assert_eq!(compat_warning(Name::Judgebot, "judgebot --api"), None);
        let w = compat_warning(Name::JudgeApi, "judgebot --api --jobs");
        assert!(
            w.as_ref()
                .is_some_and(|w| w.contains("judge-api") && w.contains("`judgebot --api --jobs`")),
            "{w:?}"
        );
    }

    /// The usage names every role.
    #[test]
    fn the_usage_lists_every_role() {
        for r in Role::ALL {
            assert!(USAGE.contains(r.flag()), "{r:?}");
        }
    }
}
