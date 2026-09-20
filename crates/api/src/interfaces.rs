//! Which front doors this process serves, and the command line that picks them.
//!
//! Every interface is opt-in. `judge-api` with no flags serves the JSON API
//! alone — the one mode the binary is named for — and the web page and the MCP
//! transport each need their own flag, so an operator who never asked to
//! publish a page never gets one. Naming any flag replaces the default rather
//! than adding to it: `--web` on its own is a page with no question route,
//! which is a thing an operator may legitimately want in front of a separate
//! API process.
//!
//! The set is a [`NonEmpty`], so "a listener bound to no interface at all" is
//! not a state this program can reach: the no-flag case *is* an interface.
//! `GET /api/health` is outside the set — it reports on the process, not on a
//! front door, and a container healthcheck must be able to reach it whatever
//! else is switched off.

use std::{ffi::OsString, fmt};

use nonempty::NonEmpty;

/// One front door of the HTTP adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    /// `POST /api/judge` — the anonymous question route.
    Api,
    /// The built web client, served from `WEB_DIST` as the router's fallback.
    Web,
    /// The MCP transport at `/mcp`, behind `MCP_TOKEN`.
    Mcp,
}

impl Interface {
    /// Every interface, in the order the usage text and the logs list them.
    /// A new variant belongs here as well as in the matches below; nothing but
    /// this comment enforces that, so [`Interfaces`] is written not to depend
    /// on it — the set it reports is the set it holds.
    pub const ALL: [Self; 3] = [Self::Api, Self::Web, Self::Mcp];

    /// The flag that enables it.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Api => "--api",
            Self::Web => "--web",
            Self::Mcp => "--mcp",
        }
    }

    /// Its name in a log line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Web => "web",
            Self::Mcp => "mcp",
        }
    }

    /// The interface a flag names, if it names one.
    #[must_use]
    fn from_flag(arg: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.flag() == arg)
    }
}

/// The interfaces this launch enables. Built only by [`parse`], which cannot
/// produce an empty set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interfaces(NonEmpty<Interface>);

impl Interfaces {
    /// What a launch with no flags serves.
    pub const DEFAULT: Interface = Interface::Api;

    /// Build a set directly. The command line is [`parse`]; this is for a
    /// caller that already knows which doors it wants (tests, or an embedder
    /// of [`crate::router`]).
    ///
    /// Duplicates collapse and the order becomes [`Interface::ALL`]'s, so two
    /// ways of naming the same doors are the same value — equality here is set
    /// equality, not "typed in the same order".
    #[must_use]
    pub fn of(interfaces: &NonEmpty<Interface>) -> Self {
        let canonical: Vec<Interface> = Interface::ALL
            .into_iter()
            .filter(|i| interfaces.contains(i))
            .collect();
        // Every variant is in `ALL`, so the filter keeps at least the head and
        // the fallback is unreachable today. It is the whole input rather than
        // part of it, so a variant that went missing from `ALL` would cost the
        // canonical order and nothing else — never a door dropped or added.
        Self(NonEmpty::from_vec(canonical).unwrap_or_else(|| interfaces.clone()))
    }

    /// Is `interface` enabled?
    #[must_use]
    pub fn has(&self, interface: Interface) -> bool {
        self.0.contains(&interface)
    }

    /// Is the anonymous question route mounted?
    #[must_use]
    pub fn api(&self) -> bool {
        self.has(Interface::Api)
    }

    /// Is the built web page served?
    #[must_use]
    pub fn web(&self) -> bool {
        self.has(Interface::Web)
    }

    /// Is the MCP transport mounted?
    #[must_use]
    pub fn mcp(&self) -> bool {
        self.has(Interface::Mcp)
    }

    /// The interfaces this launch left off, for the startup log: an operator
    /// looking for a page that is not there reads the reason here rather than
    /// guessing from a 404.
    #[must_use]
    pub fn disabled(&self) -> String {
        list(Interface::ALL.into_iter().filter(|i| !self.has(*i)))
    }
}

impl fmt::Display for Interfaces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The set itself, not `ALL` filtered by it: the log then names what is
        // mounted even for a set `ALL` does not cover. `of` has already put it
        // in `ALL` order, so two equivalent launches still log identically.
        f.write_str(&list(self.0.iter().copied()))
    }
}

fn list(interfaces: impl Iterator<Item = Interface>) -> String {
    let names: Vec<&str> = interfaces.map(Interface::name).collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// What the command line asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Print [`USAGE`] and exit successfully.
    Help,
    /// Serve these interfaces.
    Serve(Interfaces),
}

/// The help text, also printed with the error on a bad command line.
pub const USAGE: &str = "\
usage: judge-api [--api] [--web] [--mcp]

Interfaces are opt-in, and naming any replaces the default rather than adding
to it. With no flags the JSON API is served alone.

  --api   POST /api/judge, the anonymous question route
  --web   the built web page, from WEB_DIST
  --mcp   the MCP transport at /mcp (requires MCP_TOKEN)

GET /api/health and GET /api/about (the source offer) are always served.
Everything else is configured through the environment; see .env.example.";

/// Parse the arguments after the program name.
///
/// Takes [`OsString`]s, because `std::env::args()` panics on an argument that
/// is not UTF-8 and this crate denies panics: a mistyped byte should print the
/// usage like any other unknown argument.
///
/// # Errors
/// An unknown argument, or a flag given twice.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Launch> {
    let mut chosen: Vec<Interface> = vec![];
    for arg in args {
        let arg = arg.to_string_lossy();
        if arg == "-h" || arg == "--help" {
            return Ok(Launch::Help);
        }
        let Some(interface) = Interface::from_flag(&arg) else {
            anyhow::bail!("unknown argument {arg:?}\n\n{USAGE}");
        };
        // Repeats are refused rather than folded: a command line naming the
        // same door twice is a mistake, and silently accepting it hides which
        // one the operator meant to write.
        anyhow::ensure!(
            !chosen.contains(&interface),
            "{} given more than once\n\n{USAGE}",
            interface.flag()
        );
        chosen.push(interface);
    }
    Ok(Launch::Serve(NonEmpty::from_vec(chosen).map_or_else(
        || Interfaces(NonEmpty::new(Interfaces::DEFAULT)),
        |chosen| Interfaces::of(&chosen),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> anyhow::Result<Launch> {
        parse(args.iter().map(OsString::from))
    }

    fn serving(args: &[&str]) -> Option<Interfaces> {
        match parse_args(args) {
            Ok(Launch::Serve(i)) => Some(i),
            _ => None,
        }
    }

    #[test]
    fn no_flags_serves_the_json_api_alone() {
        let i = serving(&[]);
        assert_eq!(i.as_ref().map(Interfaces::api), Some(true));
        assert_eq!(i.as_ref().map(Interfaces::web), Some(false));
        assert_eq!(i.as_ref().map(Interfaces::mcp), Some(false));
        assert_eq!(i.as_ref().map(ToString::to_string), Some("api".to_owned()));
        assert_eq!(
            i.as_ref().map(Interfaces::disabled),
            Some("web, mcp".to_owned())
        );
    }

    #[test]
    fn a_flag_replaces_the_default_rather_than_adding_to_it() {
        // The whole point of the opt-in: asking for the page does not quietly
        // hand out the paid question route as well.
        let i = serving(&["--web"]);
        assert_eq!(i.as_ref().map(Interfaces::web), Some(true));
        assert_eq!(i.as_ref().map(Interfaces::api), Some(false));
        assert_eq!(i.as_ref().map(ToString::to_string), Some("web".to_owned()));
        assert_eq!(
            i.as_ref().map(Interfaces::disabled),
            Some("api, mcp".to_owned())
        );
    }

    #[test]
    fn flags_combine_and_log_in_a_stable_order() {
        let i = serving(&["--mcp", "--web", "--api"]);
        assert_eq!(
            i.as_ref().map(ToString::to_string),
            Some("api, web, mcp".to_owned())
        );
        assert_eq!(
            i.as_ref().map(Interfaces::disabled),
            Some("none".to_owned())
        );
        // Order typed must not change the meaning or the log line.
        assert_eq!(serving(&["--api", "--web", "--mcp"]), i);
    }

    #[test]
    fn help_wins_wherever_it_appears() {
        for args in [&["-h"][..], &["--help"][..], &["--web", "--help"][..]] {
            assert_eq!(parse_args(args).ok(), Some(Launch::Help), "{args:?}");
        }
    }

    #[test]
    fn unknown_and_repeated_arguments_are_refused_with_the_usage() {
        for (args, needle) in [
            (&["--serve-everything"][..], "--serve-everything"),
            (&["-w"][..], "-w"),
            (&["--web", "--web"][..], "--web"),
            (&["--api", "--mcp", "--api"][..], "--api"),
        ] {
            let r = parse_args(args);
            assert!(
                r.as_ref().is_err_and(|e| {
                    let text = format!("{e:#}");
                    text.contains(needle) && text.contains("usage: judge-api")
                }),
                "{args:?}: {r:?}"
            );
        }
    }

    #[test]
    fn a_set_built_directly_dedupes_and_canonicalises() {
        use nonempty::nonempty;
        let typed_backwards =
            Interfaces::of(&nonempty![Interface::Web, Interface::Api, Interface::Web]);
        assert_eq!(typed_backwards.to_string(), "api, web");
        assert_eq!(
            typed_backwards,
            Interfaces::of(&nonempty![Interface::Api, Interface::Web])
        );
        assert!(typed_backwards.api() && typed_backwards.web() && !typed_backwards.mcp());
    }

    /// `docker-compose.yml` passes its interfaces as a string nobody else
    /// checks: renaming a flag would leave the shipped compose file handing
    /// the binary an argument it refuses, and every test would stay green.
    /// Same guard judge.toml gets from `the_example_file_loads_as_shipped`.
    #[test]
    fn the_compose_default_is_a_command_line_this_binary_accepts() {
        use nonempty::nonempty;
        let compose = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker-compose.yml"),
        )
        .unwrap_or_default();
        let default = compose
            .lines()
            .find_map(|l| l.trim().strip_prefix("command: ${API_INTERFACES:-"))
            .and_then(|l| l.strip_suffix('}'));
        assert!(
            default.is_some(),
            "the api service does not default API_INTERFACES in docker-compose.yml"
        );
        let args = default
            .unwrap_or_default()
            .split_whitespace()
            .map(OsString::from);
        assert_eq!(
            parse(args).ok(),
            Some(Launch::Serve(Interfaces::of(&nonempty![
                Interface::Api,
                Interface::Web
            ]))),
            "docker-compose.yml passes {default:?}"
        );
    }

    #[test]
    fn every_interface_has_a_distinct_flag_and_name_and_the_usage_lists_it() {
        for i in Interface::ALL {
            assert!(i.flag().starts_with("--"), "{i:?}");
            assert_eq!(Interface::from_flag(i.flag()), Some(i));
            assert!(USAGE.contains(i.flag()), "{i:?} missing from the usage");
        }
        let flags: std::collections::BTreeSet<&str> =
            Interface::ALL.into_iter().map(Interface::flag).collect();
        assert_eq!(flags.len(), Interface::ALL.len());
    }
}
