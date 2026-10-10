//! The command line: what the binary was asked to do. Pure, so the dispatch
//! is tested without a process.
//!
//! | command | runs |
//! |---|---|
//! | `judgebot --discord --api …` | those roles (none: [`ROLES_ENV`]) |
//! | `judgebot ingest <cmd> …` | the ingest command line |
//! | `judgebot backup <cmd> …` | the database backup |

use std::ffi::OsString;

use anyhow::Result;

use judgebot::roles::{self, Origin, ROLES_ENV, Roles};

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Print this and exit successfully.
    Help(String),
    /// The ingest command line, over these arguments.
    Ingest {
        /// The arguments after `ingest`.
        args: Vec<OsString>,
    },
    /// The backup command line, over these arguments.
    Backup {
        /// The arguments after `backup`.
        args: Vec<OsString>,
    },
    /// Run roles.
    Serve {
        /// The roles, or `None` to read them from [`ROLES_ENV`].
        roles: Option<(Roles, Origin)>,
    },
}

/// The usage `judgebot --help` prints.
pub const USAGE: &str = "\
usage: judgebot <role>...
       judgebot ingest <command> [args]   (judgebot ingest --help)
       judgebot backup <command> [args]   (judgebot backup --help)

Roles: at least one, named on the command line or, with none there, in
JUDGE_ROLES (the same flags separated by spaces).

  --discord  the Discord bot (requires DISCORD_TOKEN and JUDGE_OPERATOR_DISCORD)
  --api      POST /api/judge, the anonymous question route
  --web      the built web app, from WEB_DIST (requires its index.html)
  --mcp      the MCP transport at /mcp (requires MCP_TOKEN)
  --jobs     the scheduled data refresh (JUDGE_REFRESH_HOURS; 0 turns it off)

--api, --web and --mcp require JUDGE_OPERATOR_EMAIL and listen on API_ADDR,
where GET /api/health and GET /api/about are served whichever of them is on.
Everything else is configured through the environment; see .env.example.";

/// Parse the arguments after the program name.
///
/// Takes [`OsString`]s, because `std::env::args()` panics on an argument
/// that is not UTF-8: a mistyped byte gets the usage like any other unknown
/// argument.
///
/// # Errors
/// An unknown argument or a role named twice, with the usage.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation> {
    let args: Vec<OsString> = args.into_iter().collect();
    let help = |a: &OsString| a == "-h" || a == "--help";
    if args.first().is_some_and(|a| a == "ingest") {
        return Ok(Invocation::Ingest {
            args: args.into_iter().skip(1).collect(),
        });
    }
    if args.first().is_some_and(|a| a == "backup") {
        return Ok(Invocation::Backup {
            args: args.into_iter().skip(1).collect(),
        });
    }
    if args.iter().any(help) {
        return Ok(Invocation::Help(USAGE.to_owned()));
    }
    let flags = args.iter().map(|a| a.to_string_lossy());
    Ok(Invocation::Serve {
        roles: roles::parse(flags, "command line", USAGE)?.map(|r| (r, Origin::CommandLine)),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use judgebot::roles::Role;

    fn run(args: &[&str]) -> Result<Invocation> {
        parse(args.iter().map(OsString::from))
    }

    fn served(args: &[&str]) -> Option<(String, Origin)> {
        match run(args) {
            Ok(Invocation::Serve {
                roles: Some((r, o)),
            }) => Some((r.flags(), o)),
            _ => None,
        }
    }

    #[test]
    fn judgebot_takes_roles_from_the_command_line_or_leaves_them_to_the_environment() {
        assert_eq!(
            served(&["--web", "--discord", "--jobs"]),
            Some(("--discord --web --jobs".to_owned(), Origin::CommandLine))
        );
        assert!(matches!(run(&[]), Ok(Invocation::Serve { roles: None })));
        for args in [&["--help"][..], &["-h"], &["--api", "--help"]] {
            assert!(
                matches!(run(args), Ok(Invocation::Help(u)) if u == USAGE),
                "{args:?}"
            );
        }
        for args in [
            &["--bot"][..],
            &["--api", "--api"],
            &["serve"],
            &["--api", "ingest"],
        ] {
            let r = run(args);
            assert!(
                r.as_ref()
                    .is_err_and(|e| format!("{e:#}").contains("usage: judgebot")),
                "{args:?}: {r:?}"
            );
        }
    }

    #[test]
    fn ingest_is_a_subcommand() {
        let os = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(matches!(
            run(&["ingest", "rules", "latest"]),
            Ok(Invocation::Ingest { args }) if args == os(&["rules", "latest"])
        ));
        assert!(matches!(
            run(&["ingest", "--help"]),
            Ok(Invocation::Ingest { args }) if args == os(&["--help"])
        ));
    }

    #[test]
    fn backup_is_a_subcommand() {
        let os = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(matches!(
            run(&["backup", "fetch", "x.dump.gz"]),
            Ok(Invocation::Backup { args }) if args == os(&["fetch", "x.dump.gz"])
        ));
        assert!(matches!(
            run(&["backup", "--help"]),
            Ok(Invocation::Backup { args }) if args == os(&["--help"])
        ));
        // Not a role.
        assert!(run(&["--api", "backup"]).is_err());
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

    /// The usage names every role.
    #[test]
    fn the_usage_lists_every_role() {
        for r in Role::ALL {
            assert!(USAGE.contains(r.flag()), "{r:?}");
        }
    }
}
